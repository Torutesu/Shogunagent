# V107 memory processing receipts

Adds content-free review receipts, worker heartbeat metadata and consolidation failure codes in each principal's encrypted
store. No existing migrations or rows are rewritten. Existing events have no retrospective proof
of review and status reports them as unknown when older completed cycles might have covered them.
Deleting an event deletes its receipts and failures through foreign keys.

Before upgrading, take and verify a backup. The supported rollback is to restore the pre-upgrade
store and matching keys with the previous binary; the previous binary refuses schema 107.
For an explicitly controlled schema rollback, drop the event_processing_reset trigger, then
memory_processing_runtime, event_processing_failures and event_processing_receipts, and remove
only the V107 refinery history entry. This discards processing
status evidence, not sources or typed state; never do this against a live store.
