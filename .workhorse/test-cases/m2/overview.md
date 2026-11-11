# M2: the ping schedule on the dashboard

Scenarios for the schedule tab, the slots' days, and setup leaving the wake to the dashboard.

## Slots and their days

- [x] A slot parses as a time then its days, and refuses a slot with no days (verifies spec: GPING)
- [x] Days spell as `OnCalendar=` does, ranges for runs of three or more, and read back as themselves
- [x] A ping carried past midnight runs on the day after each of its slot's days (verifies spec: GPING)
- [x] Two slots at one time are refused (verifies spec: GPING)
- [x] Slots whose pings fall within five hours on a shared day, or across the midnight between two days, are refused; exactly five hours apart is accepted (verifies spec: GPING)
- [x] The ping verb resolves a slot only on its own days, and a unit written before days reads as weekdays
- [x] The schedule reads back from the hold timer, binary included

## The user timers

- [x] Applying retires the removed slots' ping pairs, writes the rest, reloads, enables and restarts the timers
- [x] A failed systemctl call stops the apply and says what it said
- [ ] On a real machine, an edited slot's timer waits on its new time after the apply (`systemctl --user list-timers`)

## The wake

- [x] The place script carries both units whole, reloads the system manager and enables the timer, and runs nothing of Toker's (verifies spec: SEC)
- [x] The scripts write and remove the units byte for byte under a real shell
- [x] A unit line cannot close its own heredoc
- [x] pkexec's refusal and failure codes read as words on the page
- [ ] On a real machine, placing the wake prompts once through the desktop agent, and `systemctl is-enabled toker-wake.timer` reads enabled
- [ ] Over SSH with no desktop agent, the prompt appears on the terminal with the dashboard given up, and the dashboard comes back after
- [ ] Dismissing the prompt leaves the position as it was and says so in the header (verifies spec: UTUI)
- [ ] A suspended machine wakes at a slot, holds, and pings

## The schedule tab

- [x] The page at rest shows the position, each slot's days and ping time, and its newest run per account (verifies spec: UTUI)
- [x] At 80 columns the ping time and the run's time are shed before any content is cut (verifies spec: UTUI)
- [x] Space steps off, while awake, wake then ping; only moves into or out of waking ask for elevation (verifies spec: GPING)
- [x] The schedule does not turn on with no slots, nor drop its last slot while on (verifies spec: GPING)
- [x] A slot is checked as it is typed, a refused one cannot be saved, and an accepted one says when it pings (verifies spec: UTUI)
- [x] Editing a slot opens on it and judges it only against the others
- [x] While woken, a slot edit places the wake at the new slots first and writes the user timers only after (verifies spec: UTUI)
- [x] A wake with nothing to ping, the switch off or the anchoring provider off, is flagged with the way to stop it, which also turns the switch off (verifies spec: UTUI)
- [x] The keys overlay names every key the page takes
- [x] With the service down, the schedule tab reports that rather than offering edits (verifies spec: UTUI)

## Setup

- [x] Setup keeps the installed slots and their days on a re-run (verifies spec: USETUP)
- [x] Setup writes no wake, and removes the copy an earlier setup staged among the user units (verifies spec: USETUP)
