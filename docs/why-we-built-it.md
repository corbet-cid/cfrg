# Why we built cfrg

*Announcement draft. Everything below is backed by code or a test in this repository; the links say where.*

We run our projects on a Forgejo server, because we want to control where the work happens. Almost everyone who would
use, star, fork or report a bug on those projects is on GitHub or GitLab. So the projects have to be there too, and that
is where it goes wrong when you do it by hand.

Settings drift between the copies. A hosted copy starts its own automation (Actions, pipelines, Dependabot) the moment
code arrives and spends compute nobody planned. Release files get uploaded twice, or forgotten once. Issues split across
three trackers. A burst of pushes gets noticed by a provider's abuse protection. And an outside contributor's pull
request on a copy has nowhere safe to go: merging it there runs code on a machine you did not choose.

cfrg takes one position: **Forgejo is the platform; GitHub, GitLab and Bitbucket are copies.**

* **Code** is copied by Forgejo's own push mirrors. cfrg creates and verifies them rather than reinventing them
  ([native mirrors](native-mirrors.md)). A mirror is owned only when its address is a declared destination; every other
  mirror is reported and never touched.
* **Copies run nothing.** Actions, pipelines and Dependabot are switched off on every copy on every reconcile, and the
  placement format has no field that could say otherwise: a document that tries is refused
  (`cfrg/tests/it/zero_ci.rs`). The default branch of a copy is locked to the mirror.
* **Settings, members and protection** of a copy are derived from the primary: visibility, description, topics, default
  branch, who may read or triage, and whether the branch is held for the mirror alone ([native mirrors](native-mirrors.md)).
* **Writes to GitHub are paced.** One writer with budgets, a kill switch and a ledger written before each mutation
  ([gate](gate.md)).
* **Issues, comments and pull requests are relayed** between the three forges, with Forgejo as the hub and every write
  journalled first ([relay](relay.md)).
* **Outside contributions are held.** A pull or merge request from outside becomes a credited sibling request and one
  notice on the Forgejo primary. By default nothing is passed on until a human releases it, and nothing reaches the
  primary's branches until a maintainer has reviewed it and runs `cfrg take-in`, which brings in exactly the reviewed
  commit and never rebases or squashes ([take-in](take-in.md), [security model](security.md)).
* **Releases and wikis follow.** A signed release event uploads the release files to the GitHub and GitLab copies, only
  against a checksum contract and never replacing an existing file ([release upload](release-upload.md)); the wiki is
  pushed to the copies, fast-forward only ([native mirrors](native-mirrors.md)).
* **Landing is by exact green commit only** ([land](land.md)).

The pieces are small and each does one thing: `cfrg` is the command line, `cfrg-serve` the receiver that reacts to signed
events, `cfrg daily` the run a scheduler starts. The code that speaks to each forge lives in four small libraries of its own
(`cfgj`, `cghb`, `cglb`, `cbkt`) so it can be read and reused alone ([README](../README.md#the-driver-repositories)). It builds
with Nix, and the [quick start](quickstart.md) runs end to end from the flake alone.

What it does not do, so you know before you try: it does not run CI for you, it does not schedule itself, it does not
create GitLab or Bitbucket accounts, and it never deletes a destination repository. It is for projects whose primary is
Forgejo, not for projects that live on GitHub or GitLab.

cfrg is licensed LGPL-3.0-only with the linking exception, is pre-1.0 (`0.2.0`, see the [changelog](../CHANGELOG.md)), and
was built with AI assistance under human review; [CONTRIBUTING](../CONTRIBUTING.md) says how. Questions, bug reports
and pull requests go through the issue tracker.
