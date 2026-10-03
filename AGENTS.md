# Agent guidance

## Output
Keep output short and concise.

## Comments
Comments should only ever explain odd behaviour or discrepancies from expectation. Never add comments that restate what the code obviously does.

## Modules
Prefer logical modules: group related code into cohesive modules rather than flat, scattered files.

## Implementations
No hybrid implementations or side-by-side systems: when replacing behaviour, remove the old path entirely. Never keep both old and new implementations side by side.

Code can be deleted easily and written again fast — don't hoard code "just in case". Delete anything unused rather than keeping it around.

## Git delivery

Use ordinary merge commits for pull requests and branch updates. Never rebase,
squash-merge, or force-push. Fetch the current base and merge it into the topic
branch when conflicts, integration changes, or branch protection require it;
do not update every branch merely because another PR landed. Verify the current
PR head before merging.

CI reuses a successful PR run on `main` only when its recorded checkout tree
matches the merged tree in the same workflow. Missing, expired, incomplete, or
failed evidence runs the checks. Direct pushes, manual runs and schedules still
run verification; release and deployment workflows keep their own gates.
