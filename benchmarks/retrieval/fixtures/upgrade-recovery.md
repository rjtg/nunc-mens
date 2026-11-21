# Rolling upgrade and recovery

This runbook describes the on-call procedure for upgrading Sparrow workers and recovering a local queue. Start with the current release notes and the deployment ticket. Verify the migration can be applied one worker at a time and record the oldest live worker release. A mixed fleet may produce messages in both schemas. Operators coordinate the rollout through the normal release channel and keep the staging and production directories distinct.

## Before deployment

Verify the pending lease and acknowledgement counters. An environment with a large number of outstanding acknowledgements needs a drain window before restarting any worker. Export the current metrics snapshot, note the queue high-water mark and compare it with the previous checkpoint. Watch the number of retries separately from the number of messages delivered. Retries may be expected when a remote consumer is unhealthy.

### Verify staging

Run the dependency check first. It verifies that the service account can open its log and read its configuration. Confirm that the shadow workers use a separate delivery key and are unable to acknowledge production events. The same checks work for a canary, but the canary needs a live read lease. If the check fails, record its output in the deployment ticket and leave the rollout paused until the staging and production settings are reconciled.

### Reserve capacity

Keep one live validation worker available for each queue partition. A partition can run temporarily on one worker, but a second worker reduces catch-up time and allows the dispatcher to relocate jobs. For a new release, warm one standby worker per region. Set the dispatcher to accept replacements only after their metrics endpoint reports ready. For a rollback, validate that the old worker has read the most recent checkpoint before restoring its previous dispatcher settings.

## Monitor a rolling deployment

Track deliveries, retries, and deferred events together. A change in completed deliveries can reflect a change in incoming work, so compare the rate against the enqueue rate. Confirm that oldest unacknowledged age stops rising over three consecutive five-minute intervals after adding consumers. If age continues increasing while successful deliveries remain flat, pause the rollout and inspect which partition owns the unsent messages.

### Compare warning bands

A brief burst in pending retries after a worker restart is expected when a lease moves. A steady rise in deferred messages suggests a different problem. Look at how many partitions are affected and whether each queue has a standby. A single worker failing its connection check is usually limited to its assigned partition. A rising age across several regions indicates that the shared dispatcher may be down or may be rejecting a new worker's capabilities.

### Distinguish backlog from disk stalls

Read the published progress counter twice. If the counter advances and acknowledgements remain up to date, the worker is slow but processing. Review the incoming rate and temporarily raise worker capacity if it is larger than the completed rate. If the progress counter freezes while the process remains responsive, examine the log for retries against a journal segment. Disk stalls are most likely after a host replacement or when a replica must fetch a segment from a backup.

## Recover the durable queue

Run recovery from the worktree containing the live queue label. Check the root identifier against the deployment ticket. Copy the diagnostic report into an isolated directory before editing any configuration. A recovery report includes the last manifest generation, the staged generation if there was one, and the journal offset the active reader observed. Keep the fixture and production restore paths distinct during troubleshooting.

### Restore the published generation pointer

If the generation pointer is unreadable, restore `manifest.last-good.json` as the starting manifest. Validate its checksum and confirm that it refers to segments still on disk. The writer lease must be held for the restore because another process might otherwise publish a new manifest between validation and replacement. An orphaned staged manifest can contain a higher generation without having been committed. Compare the generation in the backup to the final durable checkpoint before restarting dispatch.

### Repair a replay cursor

If the offset is beyond the final durable record, stop the replay worker and reset from the newest validated checkpoint. Do not reset against a staged checkpoint. The replay cursor is an offset into a segment, and a segment may have fewer durable records than the last live writer reported before its crash.

```sh
sparrow replay-cursor inspect --queue "$queue"
sparrow replay-cursor reset --from-checkpoint --queue "$queue"
sparrow replay start --queue "$queue"
```

The reset is complete when the resumed worker reports an offset no larger than the durable high-water mark and the receiver confirms that duplicate deliveries were rejected. If the offset stalls again, preserve the checkpoint and ask the queue owner to review the segment transition in the diagnostic report.

### Escalate missing segment files

If any manifest-approved segment is missing, locate the latest consistent filesystem backup. Restoring one segment from another generation can produce a valid file that is incompatible with the current manifest. Start the rescue reader against a disposable checkout to validate the recovered bodies. Never switch the live generation until the reader completes the manifest scan and all listed segments pass their checksums.

## Finish the rollout

Wait for old worker versions to exit and the deferred message count to stabilize. Post the observed rollback point in the deployment ticket along with the new checkpoint generation. Update release notes if the recovery procedure was needed. Leave the last-known-good manifest in place through the next deployment window. Only then consider pruning staged manifests or unused segments using the maintenance command.
