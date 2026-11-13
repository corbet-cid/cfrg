# Take-in

`cfrg take-in <owner/repo>#<notice>` brings one reviewed outside contribution into the primary. Code:
`cfrg-cli/src/take_in.rs`.

A pull or merge request that arrives on a copy is relayed as a credited sibling request and a single `contribution`
notice on the Forgejo primary ([relay](relay.md)). The notice carries a marker comment as its last line:
`<!-- cfrg-contribution origin ref sha -->`, moved when the contribution gets new commits. A maintainer reviews the
contribution and says so on the notice; take-in then acts:

1. It reads the notice through the Forgejo API and refuses unless the notice is an open `contribution`, carries a valid
   marker, and has a review by the named reviewer (`--reviewer`) whose head equals the marker's newest `sha`. A
   contribution that moved after the review is refused until it is reviewed again
   (tests: `take_in::tests::notice::*`).
2. It fetches the primary's default branch and exactly the reviewed commit from the contribution's origin into a scratch
   clone, and refuses if the origin's ref is no longer that commit.
3. It fast-forwards the branch to the reviewed commit, or, when the branch has moved on, makes a merge commit. It never
   rebases, never squashes and never force-pushes, so commit identity and authorship are preserved. A merge that
   conflicts is refused and nothing is pushed (tests: `fast_forwards_when_the_primary_has_not_moved`,
   `merges_with_both_parents_when_the_primary_moved`, `refuses_when_the_head_moved_since_the_review`).
4. It pushes with a plain `git push`: if the primary moved meanwhile, git refuses and nothing is overwritten.

| Option | Meaning |
|---|---|
| `<TARGET>` | `owner/repo#number` of the notice on the Forgejo primary |
| `--forge` | Forgejo API base, for example `https://forge.example.org` |
| `--primary-host` | host that serves the primary's git URL (`<host>/<owner>/<repo>.git`); the push uses your own git credential for it |
| `--reviewer` | login whose review comments count |
| `--dry-run` | do everything except the push |

The Forgejo token that reads the notice comes from the environment variable `CFRG_FORGEJO_TOKEN`. The merge commit uses
the identity git is configured with; only when none is configured does take-in fall back to a neutral placeholder.

Once the mirrors carry the branch to the copies, the copies' requests close as merged. Only after take-in does the
primary's own CI run the commits ([security](security.md)).
