use super::*;

pub(super) fn recorded_pull(
    gitlab: &dyn GitlabBridge,
    forgejo: &dyn ForgejoBridge,
    transport: &mut dyn BridgeTransport,
    config: &Config,
    iid: u64,
    journal: &Journal,
) -> Result<(String, Entry, PullRequest)> {
    let root = journal.load(&config.key(iid))?;
    let key = generation_key(config, iid, root.generation.as_deref());
    let entry = journal.load(&key)?;
    let index = entry
        .pull_number
        .ok_or_else(|| failure("No journaled import for this MR"))?;
    let dest = preflight(gitlab, forgejo, transport, config)?;
    let pull = forgejo.pull(
        transport,
        &config.forgejo,
        &config.primary_repository,
        index,
    )?;
    let marker = format!("<!-- ccid-pr-bridge:v1:{key} -->\n");
    let branch = format!("ccid-import/gitlab/{key}");
    match_pull(std::slice::from_ref(&pull), &marker, &branch, config, &dest)?
        .ok_or_else(|| failure("Journaled PR identity changed"))?;
    if pull.number != index {
        return Err(failure("Unexpected primary PR number"));
    }
    Ok((key, entry, pull))
}

pub(super) fn feedback(
    gitlab: &dyn GitlabBridge,
    forgejo: &dyn ForgejoBridge,
    transport: &mut dyn BridgeTransport,
    config: &Config,
    iid: u64,
    journal: &Journal,
) -> Result<Value> {
    let (key, mut entry, pull) = recorded_pull(gitlab, forgejo, transport, config, iid, journal)?;
    let index = pull.number;
    if pull.state != "closed" {
        return Err(failure("Primary PR is not closed or merged"));
    }
    let source = gitlab.merge_request(transport, &config.gitlab, config.gitlab_project, iid)?;
    let mut snapshot = source.clone();
    // Validate identity even after a previous close succeeded with a lost response.
    snapshot.state = "opened".into();
    snapshot.validate(config, iid)?;
    if entry.sha.as_deref() != Some(snapshot.sha.as_str()) {
        return Err(failure(
            "Source head advanced after import; refuse to close new work",
        ));
    }
    if !matches!(source.state.as_str(), "opened" | "closed") {
        return Err(failure("Source MR has an unexpected disposition"));
    }
    let actor = gitlab.current_user_id(transport, &config.gitlab)?;
    let marker = format!("<!-- ccid-pr-bridge:feedback:{key} -->");
    let url = format!(
        "{}/{}/pulls/{index}",
        config.forgejo, config.primary_repository
    );
    let disposition = if pull.merged == Some(true) {
        "merged"
    } else if pull.merged == Some(false) {
        "closed"
    } else {
        return Err(failure("Unknown primary merge state"));
    };
    let body = format!("{marker}\nThe primary contribution was {disposition}: {url}\nClosing this secondary MR; the primary forge retains the review and merge history.");
    let notes =
        gitlab.merge_request_notes(transport, &config.gitlab, config.gitlab_project, iid)?;
    let matching: Vec<_> = notes
        .iter()
        .filter(|n| n.body.as_deref().is_some_and(|b| b.starts_with(&marker)))
        .collect();
    if matching.len() > 1
        || matching
            .iter()
            .any(|n| n.author_id != Some(actor) || n.body.as_deref() != Some(body.as_str()))
    {
        return Err(failure("Ambiguous or altered source feedback"));
    }
    if let Some(note) = matching.first() {
        let id = note
            .id
            .filter(|id| *id > 0)
            .ok_or_else(|| failure("Missing positive id"))?;
        if entry.feedback_note.is_some_and(|old| old != id) {
            return Err(failure("Source feedback identity changed"));
        }
        entry.feedback_note = Some(id);
    } else {
        if entry.feedback_attempted || entry.feedback_note.is_some() {
            return Err(failure(
                "Unresolved feedback intent; refusing duplicate comment",
            ));
        }
        entry.feedback_attempted = true;
        journal.save(&key, &entry)?;
        let note =
            gitlab.post_note(transport, &config.gitlab, config.gitlab_project, iid, &body)?;
        if note.body.as_deref() != Some(body.as_str()) || note.author_id != Some(actor) {
            return Err(failure("Feedback comment identity not confirmed"));
        }
        entry.feedback_note = Some(
            note.id
                .filter(|id| *id > 0)
                .ok_or_else(|| failure("Missing positive id"))?,
        );
    }
    journal.save(&key, &entry)?;
    // Re-read before the idempotent close. Never call GitLab's merge endpoint.
    let latest = gitlab.merge_request(transport, &config.gitlab, config.gitlab_project, iid)?;
    if latest.sha != source.sha || latest.target_branch != source.target_branch {
        return Err(failure("Source changed before closure"));
    }
    if latest.state == "opened" {
        gitlab.close_merge_request(transport, &config.gitlab, config.gitlab_project, iid)?;
    } else if latest.state != "closed" {
        return Err(failure("Source disposition changed before closure"));
    }
    let closed = gitlab.merge_request(transport, &config.gitlab, config.gitlab_project, iid)?;
    if closed.state != "closed" || closed.sha != source.sha {
        return Err(failure("Source closure not confirmed"));
    }
    Ok(
        json!({"status":"source_closed","source":config.source_url(iid),"primary":url,"disposition":disposition,"note":entry.feedback_note}),
    )
}

// Eight explicit dependencies (both adapters, transport, objects, config,
// identity, journal, head): kept separate for auditability instead of a
// context object that would hide data flow across the trust boundary.
#[allow(clippy::too_many_arguments)]
pub(super) fn replace(
    gitlab: &dyn GitlabBridge,
    forgejo: &dyn ForgejoBridge,
    transport: &mut dyn BridgeTransport,
    objects: &mut impl Objects,
    config: &Config,
    iid: u64,
    journal: &Journal,
    head: &str,
) -> Result<Value> {
    if !oid(head) {
        return Err(failure("Replacement requires an exact head SHA"));
    }
    let source = mr(gitlab, transport, config, iid)?;
    if source.sha != head {
        return Err(failure("Replacement head differs from source"));
    }
    let root_key = config.key(iid);
    let mut root = journal.load(&root_key)?;
    let old_index = if root.generation.as_deref() == Some(head) {
        root.superseded_pull
            .ok_or_else(|| failure("Missing replacement predecessor"))?
    } else {
        let (_, entry, pull) = recorded_pull(gitlab, forgejo, transport, config, iid, journal)?;
        if pull.state != "open" || pull.merged != Some(false) || entry.sha.as_deref() == Some(head)
        {
            return Err(failure(
                "Replacement requires an open PR and a changed source head",
            ));
        }
        pull.number
    };
    // New immutable generation: no force push, no history rewrite, no reused approvals.
    let result = import_generation(
        gitlab,
        forgejo,
        transport,
        objects,
        config,
        iid,
        journal,
        Some(head),
    )?;
    let index = result["pull_number"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| failure("Missing import pull number"))?;
    root.generation = Some(head.into());
    root.superseded_pull = Some(old_index);
    journal.save(&root_key, &root)?;
    let old = forgejo.pull(
        transport,
        &config.forgejo,
        &config.primary_repository,
        old_index,
    )?;
    if old.merged != Some(false) {
        return Err(failure(
            "Predecessor merged during replacement; review manually",
        ));
    }
    let backlink = format!("\n\nSuperseded after source head replacement by {}/{}/pulls/{index}. Previous reviews do not approve the replacement head.",config.forgejo,config.primary_repository);
    let body = old
        .body
        .clone()
        .ok_or_else(|| failure("Missing predecessor body"))?;
    if old.state == "open" {
        forgejo.close_pull(
            transport,
            &config.forgejo,
            &config.primary_repository,
            old_index,
            &format!("{body}{backlink}"),
        )?;
    }
    let closed = forgejo.pull(
        transport,
        &config.forgejo,
        &config.primary_repository,
        old_index,
    )?;
    if closed.state != "closed" || !closed.body.as_ref().is_some_and(|b| b.ends_with(&backlink)) {
        return Err(failure("Replacement predecessor closure not confirmed"));
    }
    Ok(
        json!({"status":"replaced","superseded_pull":old_index,"pull_number":index,"head":head,"ci":"approval_required"}),
    )
}
