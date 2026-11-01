Requirements: a title must be cut to 16 characters, not 15; add a regression
test to test.sh that pins the long-title case; the existing two tests in test.sh
must keep passing.
Files changed so far: slug.sh (cut -c1-16) and test.sh (long-title case with a
wrong expected value, so the suite currently fails).
