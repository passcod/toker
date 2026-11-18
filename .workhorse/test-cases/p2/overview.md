# In-flight stage marks in the idle column

Scenarios for the one-cell stage mark that replaces `held`/`sent`/`recv`, and the idle column that keeps its width as requests come and go.

## The stage mark

- [x] A held request shows `‖`, a sent one `⇡`, a streaming one `⇣`, each followed by its age padded to three cells (verifies spec: UTUI)
- [x] A quiet row's idle age sits behind a blank slot of the same width as a mark and its space
- [x] Ages under a minute read in seconds, and a skewed clock reads `0s`

## Parallel requests

- [x] A session with one request in flight has a plain mark; one more beside it makes the mark bold; two or more make it cyan and bold (verifies spec: UTUI)
- [x] No count is drawn, so however many requests run in parallel the cell is no wider
- [x] With `NO_COLOR` set, a session running parallel requests still shows a bold mark, while a lone request's mark stays plain
- [x] The session detail popup still lists every request in flight with its stage in words

## The table holds still

- [x] The SESSIONS header sits in the same columns with four requests in flight as with none, at 80, 100 and 120 columns (verifies spec: UTUI)
- [x] A dashboard with nothing in flight shows none of `‖`, `⇡` or `⇣`
- [ ] Watched live in `toker tui` while a session starts and finishes parallel requests, no column moves

## The legend

- [x] The `?` overlay explains `‖ ⇡ ⇣`, the bold mark and the cyan mark, and no longer carries `held sent recv` or `+N`
