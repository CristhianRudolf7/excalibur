# Browser migration checkpoint

The development workflow moved to the shared GCP development machine on 2026-10-30. The former owner and auditor guardians are disabled and no former project worker remained active at the snapshot. A Git bundle, all 23 working trees and 9,765 source/evidence files were preserved; SHA-256 comparison found no differences before continuation. Build caches, authentication material and real browser profiles were not published.

The original published browser checkpoint was7c71cc0. B5/B7/B8/B9 have bounded approved reviews; B4/B6 and full-architecture closure remain open. Interrupted B4/B6 edits are preserved in branch browser-work-b46corr at `8cc9066945ca578a7a65539b9b198e0411d01a01`. This is an unverified source checkpoint, not an approved correction. Continue that same scope after the disk failure; do not hide prior findings or claim complete product coverage.

The workflow uses Codex with Muse Spark1.3Contributor/xhigh for the principal, isolated helper workers and independent fixed-commit audits. It does not use Claude or native Meta subagents. Workflow source is published separately in the private excalibur-flows repository. Private session/state archives remain outside product Git. Planning and historical audit evidence are in ../planning/browser. Historical paths and checks describe their original snapshots and are not relabelled as new approvals.

Heavy compilation and tests share a global flock with the harness and CARGO_BUILD_JOBS=2. Business/data/permission checks remain backend tests; frontend review is visual on small and large screens only. The migration itself does not certify all desktop targets or authenticated long-session campaigns.
