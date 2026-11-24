#import "lib.typ": *

= How this report was produced <app-method>

The report describes one commit, #raw(pinned), read from a clean export so that uncommitted work can't leak into it. Its first version described an earlier commit, `7898396`, and it has been brought up to date since. At `0fa41b6`, which added device interrupts and the keyboard lights, every listing was re-pinned by script and checked again, and the outputs of the crates that changed, the host tests and both kernel suites were recorded again. #raw(pinned) changes only the keyboard lights, after a run on the laptop. Its listings were checked again, and the lights' outputs and the count of host tests were recorded from it. The other outputs are from `0fa41b6`, or from `79b31d6` for crates with no change since. Four scripts in `docs/` keep it honest, and each can be run again.

#dtable(columns: (auto, 1fr), ("Script", "What it does"),
  [`record.sh`], [Exports the pinned commit, and runs the commands in `record.d/` to produce the outputs in `output/`],
  [`tools/check-snippets.py`], [Compares every listing with the pinned commit, line by line, ignoring whitespace],
  [`tools/check-quotes.py`], [Finds every line of every quoted output in that section's recordings],
  [`tools/check-coverage.py`], [Lists any module, public item or kernel of the pinned commit that the report doesn't name],
)

`tools/expand.py` writes the listings in the first place, from a marker naming a file and line ranges, so that no listing is typed by hand.

At `7898396`, every recording script was run a second time from a fresh export, and the report quoted the first run. Of the files recorded, 36 came out different. The differences are times and what follows from them (load averages, timer ticks counted, how often one allocator beat another in `runs`, and a page check of `green-tea-measure` that failed intermittently at that commit, and has since been fixed), addresses that the operating system randomizes, which hart OpenSBI booted on, log timestamps, process ids and build paths, and callgrind's totals, by at most 39 instructions in 665 million. Every kernel passed again. That comparison was not repeated at #raw(pinned). The tables of times in this report are examples of one run.

The coverage check is a floor. It proves each public item is named in its crate's section, and can't prove that what the section says about it is right. Claims were checked against the code and the recordings as they were written.

The crates were written up in parallel by AI agents working from one brief, with `green-tea-pool` and `green-tea-fit` as models, and then read through as a whole. The defects of @app-issues were found in that first pass, at `7898396`, and each was checked a second time, independently of the section that found it, by reading the cited code and, wherever that was cheap, by running a small program. The same method of agents and checkers was used for the update.
// chapter-status:begin
// stage: research-done
// revision: 4
// body: bad0f9a4f264
// activity: 2026-11-22 | r3 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r3 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
