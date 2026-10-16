**SIGN-OFF** on `plan-consensus.md` as written.

I accept all three changes, including P0b entropy-backed choice helpers followed by the P1a engine-selected choice protocol. That defers the **choice-specific** request/reply protocol; flush and summary-query replies remain P0a requirements. All previous sign-off conditions are satisfied.

The decision log fairly represents my positions. Three wording corrections apply **only to the log**:

1. In §5, replace:

   > existing `park` rejects owed I/O and records a witness marker …

   With:

   > Both adapters guard parking against owed I/O. KVM additionally records a `P` witness marker; ARM’s physical park validates stopped state. Codex clarified this backend distinction in round 2.

2. In §6, replace:

   > a lying flush tests nothing software can survive;

   With:

   > lying flush should be excluded from the baseline; the initial blanket impossibility argument was later withdrawn by both planners (see §11);

3. In §6, replace:

   > Codex conceded (round 1) opt-in corruption as P2, citing Opus's FAST'17 single-replica-corruption bug class. It requires two separate contracts (corrupt-on-read versus stable mutation) and stated redundancy properties.

   With:

   > Codex accepted opt-in corruption as P2, with separate corrupt-on-read and stable-mutation contracts and stated redundancy properties. Opus supplied the FAST’17 example; Codex did not independently verify that paper.