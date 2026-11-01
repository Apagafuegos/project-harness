Requirements: rename clamp to clamp_range in lib/util.sh and fix every call site;
keep the output of main.sh identical; do not rename add in lib/math.sh.
Files changed so far: lib/util.sh (definition renamed) and main.sh (one of two
call sites renamed, so main.sh currently fails).
