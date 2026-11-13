# Why we built cfrg

We keep our projects on a Forgejo server because we want to control where the
work happens. Our users and contributors, though, are on GitHub and GitLab, so
the projects have to be there too.

Doing that by hand does not last. Settings drift between copies. Hosted copies
start their own automation (Actions, pipelines, Dependabot) the moment code
arrives, and spend compute we did not plan to spend. A burst of pushes can
trigger a provider's abuse protections.

cfrg takes one position: Forgejo is the platform, the others are copies.

* Code is copied by Forgejo's own push mirrors; cfrg only configures and verifies
  them ([native mirrors](native-mirrors.md)).
* Copies never run CI, and the placement format cannot express otherwise
  (`cfrg/tests/it/zero_ci.rs`).
* GitHub writes go through one paced writer with budgets and a kill switch
  ([gate](gate.md)).
* Landing is by exact green commit only ([land](land.md)).

What comes next, not yet built: relaying issues and pull requests between forges
(B2, B3, B8), a signed receiver for GitHub and GitLab events (B5), release
upload to copies (B5) and take-in of outside contributions after human review
(B13). Binaries, a container image and a Nix flake follow (B14).

cfrg is licensed LGPL-3.0-only with the linking exception, is pre-1.0, and was
built with AI assistance under human review (see [CONTRIBUTING](../CONTRIBUTING.md)).
Questions and reports go through the issue tracker.
