# Contributed projects

A **contributed project** is someone else's project that we contribute to (operating model R10, R11).
Its own CI and conventions apply. We do not mirror or relay its releases and issues, our compute
does not run its CI, and nothing of ours is put into its repository. We keep our own copy of it
(a fork) where that helps our work.

## The declaration

The placement document (`lib/placement.json`, the one declared placement) lists the projects in a
`contributed` list of its `native` view, next to `repositories`:

```json
"contributed": [
  {"name": "seaweedfs",
   "upstream": "https://github.com/seaweedfs/seaweedfs",
   "forks": ["https://github.com/example-user/seaweedfs", "https://forge.example/example-user/seaweedfs"]}
]
```

| Field | Meaning |
|---|---|
| `name` | A plain lowercase name, unique; the list is in strictly ascending order. |
| `upstream` | The project's own repository. |
| `forks` | Optional. Our copies of it on any forge, ours included. |

Every location is spelled `https://<host>/<owner>/<repo>` exactly: lowercase host, no user name,
port, query, trailing slash or `.git`. A location belongs to one project; owner and repository match
case-insensitively. A location on the placement's source forge is known under every spelling of that
forge (the hosts of `source.origin` and `default`, and any the caller names). Loading a placement
(`Placement::validate`) refuses a malformed entry with the entry named.

ccid reads the same list with the same rules (its `docs/contributed.md`): the class is declared once.
Without the list nothing is classified and every document reads as before.

## What the declaration does

| Effect | Where |
|---|---|
| **No mirror.** The placement is invalid when a contributed project's repository has a live `destination`, and when any live `destination` (its provider, host and path, pinned or following the primary) is itself a contributed project. A destination marked `absent` is a mirror being retired and is allowed: it is how an existing mirror of a project that has become contributed goes (mark it `absent` with its `remote_name`, reconcile, then drop the entry). A contributed project is the explicit exclusion that operating model R2 asks for. | `Placement::validate` |
| **A guard for the callers that relay or upload.** `cfrg::contributed::Contributed` answers whether `owner/repo` on a forge is a contributed project (`classify`; a provider's API origin names its web host), and `refuse(operation, forge_host, path)` turns it into an error naming the operation. **Nothing in cfrg calls it yet**: the router (RD12) must call it for the primary and for every copy before any write, and the release upload of the bridge service (R45) before an upload. `cfrg bridge` (the old GitLab merge-request import, retired by RL7) and `cfrg sync` (which reads its own policy document, never this one) do not consult it. | `cfrg/src/contributed.rs` |
| **No CI of theirs on our runners.** `cfrg native` keeps the **Actions unit off** on every contributed repository on the source forge. A fork carries the upstream's workflows, and Forgejo reads `.github/workflows` when there is no `.forgejo`, so with Actions on, their CI would run on our runners. | `cfrg native` |

`default` names the default primary, which a primary switch can point at another forge, so it is no
spelling of our forge: only the source's host is, plus the spellings a caller names. List a fork under every
spelling it is cloned by (`https://forge.example/o/r` and `https://git.example/o/r`) where the caller does
not know both.

### The Actions pass

It runs with every `cfrg native` operation except `names`, `rename` and `sync-now`, and only without
`--provider`. It runs **before** the declared repositories, and one repository that fails to answer does not
stop the others.

| Operation | Result per contributed repository on the source forge |
|---|---|
| `plan`, `status`, or `reconcile` without `--apply` | `{"source":"owner/repo","state":"contributed","actions":"on"\|"off"\|"absent"\|"error"}`. Anything but `off` makes the run incomplete. |
| `reconcile --apply` | Actions found on are switched off (`PATCH has_actions=false`) and read back: `"actions":"disabled"`. Off stays off. Nothing else is changed. |

`absent` is open, not safe: Forgejo answers 404 for a repository that does not exist and for one the credential
cannot see, so a declared repository that cannot be found is a wrong path or a credential that is too narrow.

What it does not do: it acts when `cfrg native` runs (a reconcile or a status check), not when a fork is
created, so a new fork has Actions on until the next run. It switches the unit of forks on **our** forge only;
the Actions or pipelines of a fork on GitHub, GitLab or Bitbucket are that forge's own switch (R6, R7, R9: a
copy runs no CI there; the GitHub forks declared today have Actions disabled).

`--repository owner/repo` selects one contributed repository like any declared one. The copy policy
(`copy`, our own backup to the plain git host) is not a forge mirror and still covers every repository
the source lists, a contributed fork included.
