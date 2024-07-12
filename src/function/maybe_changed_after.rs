use arc_swap::Guard;

use crate::{
    key::DatabaseKeyIndex,
    runtime::{
        local_state::{ActiveQueryGuard, EdgeKind, QueryOrigin},
        StampedValue,
    },
    storage::DatabaseGen,
    Id, Revision, Runtime,
};

use super::{memo::Memo, Configuration, IngredientImpl};

impl<C> IngredientImpl<C>
where
    C: Configuration,
{
    pub(super) fn maybe_changed_after<'db>(
        &'db self,
        db: &'db C::DbView,
        key: Id,
        revision: Revision,
    ) -> bool {
        let runtime = db.runtime();
        runtime.unwind_if_revision_cancelled(db);

        loop {
            let database_key_index = self.database_key_index(key);

            log::debug!("{database_key_index:?}: maybe_changed_after(revision = {revision:?})");

            // Check if we have a verified version: this is the hot path.
            let memo_guard = self.memo_map.get(key);
            if let Some(memo) = &memo_guard {
                if self.shallow_verify_memo(db, runtime, database_key_index, memo) {
                    return memo.revisions.changed_at > revision;
                }
                drop(memo_guard); // release the arc-swap guard before cold path
                if let Some(mcs) = self.maybe_changed_after_cold(db, key, revision) {
                    return mcs;
                } else {
                    // We failed to claim, have to retry.
                }
            } else {
                // No memo? Assume has changed.
                return true;
            }
        }
    }

    fn maybe_changed_after_cold<'db>(
        &'db self,
        db: &'db C::DbView,
        key_index: Id,
        revision: Revision,
    ) -> Option<bool> {
        let runtime = db.runtime();
        let database_key_index = self.database_key_index(key_index);

        let _claim_guard = self
            .sync_map
            .claim(db.as_salsa_database(), database_key_index)?;
        let active_query = runtime.push_query(database_key_index);

        // Load the current memo, if any. Use a real arc, not an arc-swap guard,
        // since we may recurse.
        let old_memo = match self.memo_map.get(key_index) {
            Some(m) => Guard::into_inner(m),
            None => return Some(true),
        };

        log::debug!(
            "{database_key_index:?}: maybe_changed_after_cold, successful claim, \
            revision = {revision:?}, old_memo = {old_memo:#?}",
        );

        // Check if the inputs are still valid and we can just compare `changed_at`.
        if self.deep_verify_memo(db, &old_memo, &active_query) {
            return Some(old_memo.revisions.changed_at > revision);
        }

        // If inputs have changed, but we have an old value, we can re-execute.
        // It is possible the result will be equal to the old value and hence
        // backdated. In that case, although we will have computed a new memo,
        // the value has not logically changed.
        if old_memo.value.is_some() {
            let StampedValue { changed_at, .. } = self.execute(db, active_query, Some(old_memo));
            return Some(changed_at > revision);
        }

        // Otherwise, nothing for it: have to consider the value to have changed.
        Some(true)
    }

    /// True if the memo's value and `changed_at` time is still valid in this revision.
    /// Does only a shallow O(1) check, doesn't walk the dependencies.
    #[inline]
    pub(super) fn shallow_verify_memo(
        &self,
        db: &C::DbView,
        runtime: &Runtime,
        database_key_index: DatabaseKeyIndex,
        memo: &Memo<C::Value<'_>>,
    ) -> bool {
        let verified_at = memo.verified_at.load();
        let revision_now = runtime.current_revision();

        log::debug!("{database_key_index:?}: shallow_verify_memo(memo = {memo:#?})",);

        if verified_at == revision_now {
            // Already verified.
            return true;
        }

        if memo.check_durability(runtime) {
            // No input of the suitable durability has changed since last verified.
            let db = db.as_salsa_database();
            memo.mark_as_verified(db, runtime, database_key_index);
            memo.mark_outputs_as_verified(db, database_key_index);
            return true;
        }

        false
    }

    /// True if the memo's value and `changed_at` time is up to date in the current
    /// revision. When this returns true, it also updates the memo's `verified_at`
    /// field if needed to make future calls cheaper.
    ///
    /// Takes an [`ActiveQueryGuard`] argument because this function recursively
    /// walks dependencies of `old_memo` and may even execute them to see if their
    /// outputs have changed. As that could lead to cycles, it is important that the
    /// query is on the stack.
    pub(super) fn deep_verify_memo(
        &self,
        db: &C::DbView,
        old_memo: &Memo<C::Value<'_>>,
        active_query: &ActiveQueryGuard<'_>,
    ) -> bool {
        let runtime = db.runtime();
        let database_key_index = active_query.database_key_index;

        log::debug!("{database_key_index:?}: deep_verify_memo(old_memo = {old_memo:#?})",);

        if self.shallow_verify_memo(db, runtime, database_key_index, old_memo) {
            return true;
        }

        match &old_memo.revisions.origin {
            QueryOrigin::Assigned(_) => {
                // If the value was assigneed by another query,
                // and that query were up-to-date,
                // then we would have updated the `verified_at` field already.
                // So the fact that we are here means that it was not specified
                // during this revision or is otherwise stale.
                //
                // Example of how this can happen:
                //
                // Conditionally specified queries
                // where the value is specified
                // in rev 1 but not in rev 2.
                return false;
            }
            QueryOrigin::BaseInput => {
                // This value was `set` by the mutator thread -- ie, it's a base input and it cannot be out of date.
                return true;
            }
            QueryOrigin::DerivedUntracked(_) => {
                // Untracked inputs? Have to assume that it changed.
                return false;
            }
            QueryOrigin::Derived(edges) => {
                // Fully tracked inputs? Iterate over the inputs and check them, one by one.
                //
                // NB: It's important here that we are iterating the inputs in the order that
                // they executed. It's possible that if the value of some input I0 is no longer
                // valid, then some later input I1 might never have executed at all, so verifying
                // it is still up to date is meaningless.
                let last_verified_at = old_memo.verified_at.load();
                for &(edge_kind, dependency_index) in edges.input_outputs.iter() {
                    match edge_kind {
                        EdgeKind::Input => {
                            if dependency_index
                                .maybe_changed_after(db.as_salsa_database(), last_verified_at)
                            {
                                return false;
                            }
                        }
                        EdgeKind::Output => {
                            // Subtle: Mark outputs as validated now, even though we may
                            // later find an input that requires us to re-execute the function.
                            // Even if it re-execute, the function will wind up writing the same value,
                            // since all prior inputs were green. It's important to do this during
                            // this loop, because it's possible that one of our input queries will
                            // re-execute and may read one of our earlier outputs
                            // (e.g., in a scenario where we do something like
                            // `e = Entity::new(..); query(e);` and `query` reads a field of `e`).
                            //
                            // NB. Accumulators are also outputs, but the above logic doesn't
                            // quite apply to them. Since multiple values are pushed, the first value
                            // may be unchanged, but later values could be different.
                            // In that case, however, the data accumulated
                            // by this function cannot be read until this function is marked green,
                            // so even if we mark them as valid here, the function will re-execute
                            // and overwrite the contents.
                            database_key_index.mark_validated_output(
                                db.as_salsa_database(),
                                dependency_index.try_into().unwrap(),
                            );
                        }
                    }
                }
            }
        }

        old_memo.mark_as_verified(db.as_salsa_database(), runtime, database_key_index);
        true
    }
}
