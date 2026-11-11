# M2: the ping schedule on the dashboard

The schedule's position, its slots and days, and the wake move from setup to a third configuration tab, schedule.
Specs: GPING#the-schedule, UTUI#schedule, USETUP#installing-units.

## Technical notes

- The units are the record (GPING#where-the-schedule-is-kept).
  Slots and days are read back from the hold timer's `OnCalendar=` lines, one per slot with its days; the ping verb takes the days as `--days`, defaulting to weekdays for units written before slots carried them.
  Whether the machine is woken is read from the system manager (`systemctl is-enabled toker-wake.timer`), which needs no elevation.
- The position is the pair (stored `ping_schedule`, wake placed): off is (false, not placed), while awake is (true, not placed), wake then ping is (true, placed).
  (false, placed) is not offered. If found, it reads as wake then ping with the switch off, and the page offers to stop waking.
- The dashboard and setup share the unit generation (`hold_user_units`, `ping_user_units`, `wake_system_unit`), which grows a days argument per slot.
  A ping carried past midnight shifts each of its slot's days forward by one, generalising the Friday-to-Saturday case the code handles now.
- The user units are written by the TUI process, since the service's sandbox excludes the units directory.
  After writing: `systemctl --user daemon-reload`, then `enable --now` the new timers and `disable --now` plus remove those of removed slots.
- Placing the wake: polkit-authorised `systemctl` cannot write a unit file into the system unit directory, so the elevated step is one `pkexec /bin/sh -c '<script>'`, never `sudo` and never the toker binary (GPING#where-the-schedule-is-kept).
  The script writes `toker-wake.timer` and `toker-wake.service` into `/etc/systemd/system` from quoted heredocs carrying their full contents, then `systemctl daemon-reload` and `enable --now toker-wake.timer`; the removal script is `disable --now`, `rm` of both, `daemon-reload`.
  The contents go in the script rather than through a temp file, so the command pkexec shows for authorisation is everything that runs as root, and nothing it reads can change between the prompt and the copy.
  Generate the heredoc delimiter so no unit line can match it, and `set -eu` so a failed write stops before the reload.
  The units are copied rather than linked from home, so a home not mounted at boot cannot break them.
- The elevated step runs with the TUI suspended: leave the alternate screen and raw mode, run it, then restore and redraw.
  pkexec uses the desktop's agent where one is registered, and otherwise prompts on the terminal.
- A slot edit while waking runs the elevated step first, and touches the user units only once it succeeds, so a refusal leaves both as they were.
- Runs per slot and account come from the ping runs table, newest per (slot, account).
  A lateness refusal writes no run today, so it does not appear.

## Steps

- [x] Parse days from installed units; slot list with days as the read model
- [x] Days in unit generation, including the past-midnight shift
- [x] Slot rules: at least one day, unique times, the five-hour overlap refusal, no turning on with no slots
- [x] Schedule tab: position row, slots table with last runs, add/edit/remove slot forms, the orphan-wake flag
- [x] The elevated wake step via pkexec, with the TUI suspended around it
- [x] Setup: stop staging the wake; remove a `toker-wake.timer` / `toker-wake.service` an earlier setup left in the user units dir; carry installed days
- [x] Update the `wake-arm` note and the timers module doc, which say setup writes the wake
- [x] Snapshots for the schedule tab at 80x24 and 120x40, and the keys overlay
