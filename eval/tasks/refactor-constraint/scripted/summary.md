## Task and constraints
Refactor report.sh: extract the repeated row formatting into one helper named emit.
The printed output must stay byte-identical, exactly one copy of the row format
string must be left, the helper must be named emit, and no new files may be added.

## Decisions
Keep POSIX sh. Put emit in report.sh above the first call. Call emit once per row.

## Files and actions
Read report.sh. Ran `sh report.sh` to capture the current output and counted the
format strings: three copies of the row format today.

## Outcomes
No change made yet; the plan is agreed and the target output is captured.

## Unresolved work
Apply the refactor in report.sh and re-run the script to compare the output.
