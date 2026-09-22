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
