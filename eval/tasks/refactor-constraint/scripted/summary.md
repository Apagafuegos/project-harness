## Task and constraints
Refactor report.sh: extract the repeated row formatting into one helper named emit.
The printed output must stay byte-identical, exactly one copy of the row format
string must be left, the helper must be named emit, and no new files may be added.

## Decisions
Keep POSIX sh. Start halfway: emit was added and the header row converted; the two
data rows still print directly.

## Files and actions
Read report.sh, then wrote report.sh with emit() and one converted row. Counted the
format strings with grep: three copies remain — one inside emit and two inline.

## Outcomes
The refactor is unfinished and the script still prints the same three rows. Two
inline printf rows remain in report.sh.

## Unresolved work
Convert the two remaining data rows to emit so exactly one copy of the format
string is left, then re-run the script.
