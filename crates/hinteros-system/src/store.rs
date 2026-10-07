//! `apps.install` / `apps.remove` (M8, catalog.md): what a request means for `state.json`,
//! checked against the running generation's catalog index. Pure functions over the typed
//! state and the index, so every rule is unit-tested here; the service does the I/O.

use std::collections::{BTreeMap, BTreeSet};

use hinteros_api::catalog::{self, Index, Status};
use hinteros_api::{ErrorCode, ErrorObject};
use serde_json::json;

/// What an install adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// New ids in install order: the requested ones, then their dependencies.
    pub new: Vec<String>,
    /// Sum of the new apps' `sizeMiB`.
    pub size_mib: u64,
    /// (id, terms version) of the new apps that have vendor terms (all accepted).
    pub terms: Vec<(String, String)>,
}

/// Checks shared by both methods: 1 to 8 ids, each a catalog id, duplicates dropped.
pub fn check_ids(ids: &[String]) -> Result<Vec<String>, ErrorObject> {
    if ids.is_empty() || ids.len() > catalog::MAX_IDS {
        return Err(ErrorObject::invalid_field(
            "ids",
            "out_of_range",
            format!("between 1 and {} ids", catalog::MAX_IDS),
        ));
    }
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if !catalog::is_app_id(id) {
            return Err(ErrorObject::invalid_field(
                "ids",
                "bad_format",
                "ids must match ^[a-z0-9][a-z0-9-]{0,31}$",
            ));
        }
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    Ok(out)
}

/// Check `accepted_terms`' shape: at most 8 entries, catalog ids to terms versions.
pub fn check_accepted_terms(accepted: &BTreeMap<String, String>) -> Result<(), ErrorObject> {
    if accepted.len() > catalog::MAX_IDS {
        return Err(ErrorObject::invalid_field(
            "accepted_terms",
            "out_of_range",
            "at most 8 entries",
        ));
    }
    for (id, version) in accepted {
        if !catalog::is_app_id(id) || !catalog::is_terms_version(version) {
            return Err(ErrorObject::invalid_field(
                "accepted_terms",
                "bad_format",
                "accepted_terms maps catalog ids to terms versions",
            ));
        }
    }
    Ok(())
}

fn not_found(message: String) -> ErrorObject {
    ErrorObject::new(ErrorCode::NotFound, message)
}

pub fn conflict(id: &str, with: &str, message: String) -> ErrorObject {
    ErrorObject::with_data(
        ErrorCode::AppConflict,
        message,
        json!({ "id": id, "conflicts_with": with }),
    )
}

/// Plan `apps.install {ids, accepted_terms}` for a box with `installed` apps.
pub fn plan_install(
    index: &Index,
    installed: &[String],
    ids: &[String],
    accepted_terms: &BTreeMap<String, String>,
) -> Result<InstallPlan, ErrorObject> {
    let ids = check_ids(ids)?;
    check_accepted_terms(accepted_terms)?;
    for id in &ids {
        let entry = index
            .get(id)
            .ok_or_else(|| not_found(format!("{id} is not in this system's catalog")))?;
        // An installed app stays installed whatever its status; a new one must be available.
        if entry.status != Status::Available && !installed.contains(id) {
            return Err(ErrorObject::invalid_field(
                "ids",
                "not_available",
                format!("{id} can no longer be installed"),
            ));
        }
    }
    let wanted = index
        .with_dependencies(&ids)
        .map_err(|id| not_found(format!("{id} is not in this system's catalog")))?;
    let new: Vec<String> = wanted
        .into_iter()
        .filter(|id| !installed.contains(id))
        .collect();
    for id in &new {
        let entry = index.get(id).expect("checked by with_dependencies");
        if entry.status != Status::Available {
            return Err(ErrorObject::invalid_field(
                "ids",
                "not_available",
                format!("{id} (a dependency) can no longer be installed"),
            ));
        }
    }
    // Conflicts, in both directions, with what is installed and with each other.
    let after: BTreeSet<&String> = installed.iter().chain(&new).collect();
    for id in &new {
        let entry = index.get(id).expect("in the index");
        for other in &after {
            if *other == id {
                continue;
            }
            let theirs = index
                .get(other)
                .is_some_and(|o| o.conflicts.iter().any(|c| c == id));
            if entry.conflicts.iter().any(|c| c == *other) || theirs {
                return Err(conflict(id, other, format!("{id} conflicts with {other}")));
            }
        }
    }
    // Terms: the accepted version must be the catalog's, for every new app that has terms.
    let mut terms = Vec::new();
    for id in &new {
        if let Some(t) = &index.get(id).expect("in the index").terms {
            if accepted_terms.get(id) != Some(&t.version) {
                return Err(ErrorObject::with_data(
                    ErrorCode::TermsRequired,
                    format!("{id} needs its terms accepted (version {})", t.version),
                    json!({ "id": id, "version": t.version }),
                ));
            }
            terms.push((id.clone(), t.version.clone()));
        }
    }
    // Closed input: accepted terms only for apps this request installs.
    if let Some(stray) = accepted_terms.keys().find(|k| !new.contains(k)) {
        return Err(ErrorObject::invalid_field(
            &format!("accepted_terms.{stray}"),
            "not_installed_here",
            format!("{stray} is not installed by this request"),
        ));
    }
    let size_mib = new
        .iter()
        .map(|id| index.get(id).map_or(0, |e| e.size_mib))
        .sum();
    Ok(InstallPlan {
        new,
        size_mib,
        terms,
    })
}

/// Plan `apps.remove {ids}`: the ids to drop from `installed`.
pub fn plan_remove(
    index: Option<&Index>,
    installed: &[String],
    ids: &[String],
) -> Result<Vec<String>, ErrorObject> {
    let ids = check_ids(ids)?;
    for id in &ids {
        if !installed.contains(id) {
            return Err(not_found(format!("{id} is not installed")));
        }
    }
    // Nothing that stays may depend on what goes.
    if let Some(index) = index {
        for stays in installed.iter().filter(|i| !ids.contains(i)) {
            let Some(entry) = index.get(stays) else {
                continue;
            };
            if let Some(dep) = entry.depends.iter().find(|d| ids.contains(d)) {
                return Err(conflict(dep, stays, format!("{stays} depends on {dep}")));
            }
        }
    }
    Ok(ids)
}

/// The data paths of the apps being removed (from the running generation's catalog, checked
/// again here: relative, no `..`, no wildcards).
pub fn data_paths(index: &Index, ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for id in ids {
        if let Some(e) = index.get(id) {
            for p in &e.data_paths {
                if catalog::is_data_path(p) && !out.contains(p) {
                    out.push(p.clone());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        Index::parse(
            br#"{"indexVersion":2,"apps":[
              {"id":"steam","name":"Steam","status":"available","sizeMiB":1000,
               "dataPaths":[".local/share/Steam",".steam"],
               "terms":{"title":"SSA","version":"2026-09-10","url":"https://x","retrieved":"2026-10-06","file":"terms/steam-2026-09-10.txt"}},
              {"id":"moonlight","name":"Moonlight","status":"available","sizeMiB":150},
              {"id":"firefox","name":"Firefox","status":"available","sizeMiB":10,"dataPaths":[".mozilla"]},
              {"id":"youtube","name":"YouTube","status":"available","sizeMiB":1,"depends":["firefox"],
               "dataPaths":[".local/share/hinteros/firefox/youtube"]},
              {"id":"kodi","name":"Kodi","status":"available","sizeMiB":200,"conflicts":["jellyfin"]},
              {"id":"jellyfin","name":"Jellyfin","status":"available","sizeMiB":100},
              {"id":"old","name":"Old","status":"retired","statusReason":"x"},
              {"id":"gone","name":"Gone","status":"removed","statusReason":"x"},
              {"id":"broken","name":"Broken","status":"available","depends":["gone"]}
            ]}"#,
            "/idx".into(),
        )
        .unwrap()
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn code(e: &ErrorObject) -> Option<ErrorCode> {
        e.kind()
    }

    #[test]
    fn install_adds_dependencies_and_skips_installed() {
        let i = index();
        let none = BTreeMap::new();
        let p = plan_install(&i, &[], &ids(&["youtube"]), &none).unwrap();
        assert_eq!(p.new, ["youtube", "firefox"]);
        assert_eq!(p.size_mib, 11);
        let p = plan_install(&i, &ids(&["firefox"]), &ids(&["youtube", "youtube"]), &none).unwrap();
        assert_eq!(p.new, ["youtube"]);
        let p = plan_install(&i, &ids(&["moonlight"]), &ids(&["moonlight"]), &none).unwrap();
        assert!(p.new.is_empty(), "nothing to do");
    }

    #[test]
    fn install_refuses() {
        let i = index();
        let none = BTreeMap::new();
        let e = plan_install(&i, &[], &ids(&["nope"]), &none).unwrap_err();
        assert_eq!(code(&e), Some(ErrorCode::NotFound));
        for bad in [&["old"][..], &["gone"], &["broken"]] {
            let e = plan_install(&i, &[], &ids(bad), &none).unwrap_err();
            assert_eq!(code(&e), Some(ErrorCode::InvalidParams), "{bad:?}");
        }
        for bad in [
            &[][..],
            &["../x"],
            &["Steam"],
            &["a", "b", "c", "d", "e", "f", "g", "h", "i"],
        ] {
            let e = plan_install(&i, &[], &ids(bad), &none).unwrap_err();
            assert_eq!(code(&e), Some(ErrorCode::InvalidParams), "{bad:?}");
        }
        // Conflicts both ways, and among the requested apps.
        let e = plan_install(&i, &ids(&["jellyfin"]), &ids(&["kodi"]), &none).unwrap_err();
        assert_eq!(code(&e), Some(ErrorCode::AppConflict));
        assert_eq!(
            e.data.unwrap(),
            json!({"id": "kodi", "conflicts_with": "jellyfin"})
        );
        let e = plan_install(&i, &ids(&["kodi"]), &ids(&["jellyfin"]), &none).unwrap_err();
        assert_eq!(
            e.data.unwrap(),
            json!({"id": "jellyfin", "conflicts_with": "kodi"})
        );
        let e = plan_install(&i, &[], &ids(&["kodi", "jellyfin"]), &none).unwrap_err();
        assert_eq!(code(&e), Some(ErrorCode::AppConflict));
    }

    #[test]
    fn terms_must_be_the_current_version() {
        let i = index();
        let e = plan_install(&i, &[], &ids(&["steam"]), &BTreeMap::new()).unwrap_err();
        assert_eq!(code(&e), Some(ErrorCode::TermsRequired));
        assert_eq!(
            e.data.unwrap(),
            json!({"id": "steam", "version": "2026-09-10"})
        );
        let old = BTreeMap::from([("steam".to_string(), "2025-01-01".to_string())]);
        assert_eq!(
            code(&plan_install(&i, &[], &ids(&["steam"]), &old).unwrap_err()),
            Some(ErrorCode::TermsRequired)
        );
        let ok = BTreeMap::from([("steam".to_string(), "2026-09-10".to_string())]);
        let p = plan_install(&i, &[], &ids(&["steam"]), &ok).unwrap();
        assert_eq!(p.terms, [("steam".to_string(), "2026-09-10".to_string())]);
        // Terms for an app this request does not install: closed input.
        let stray = BTreeMap::from([
            ("steam".to_string(), "2026-09-10".to_string()),
            ("moonlight".to_string(), "1".to_string()),
        ]);
        let e = plan_install(&i, &[], &ids(&["steam"]), &stray).unwrap_err();
        assert_eq!(e.data.unwrap()["field"], "accepted_terms.moonlight");
        let bad = BTreeMap::from([("steam".to_string(), "../x".to_string())]);
        assert_eq!(
            code(&plan_install(&i, &[], &ids(&["steam"]), &bad).unwrap_err()),
            Some(ErrorCode::InvalidParams)
        );
    }

    #[test]
    fn remove_rules() {
        let i = index();
        let installed = ids(&["firefox", "youtube", "steam", "gone"]);
        assert_eq!(
            plan_remove(Some(&i), &installed, &ids(&["steam"])).unwrap(),
            ["steam"]
        );
        let e = plan_remove(Some(&i), &installed, &ids(&["moonlight"])).unwrap_err();
        assert_eq!(code(&e), Some(ErrorCode::NotFound));
        let e = plan_remove(Some(&i), &installed, &ids(&["firefox"])).unwrap_err();
        assert_eq!(
            e.data.unwrap(),
            json!({"id": "firefox", "conflicts_with": "youtube"})
        );
        // Together with its dependent it can go; so can a tombstone.
        assert!(plan_remove(Some(&i), &installed, &ids(&["firefox", "youtube"])).is_ok());
        assert!(plan_remove(Some(&i), &installed, &ids(&["gone"])).is_ok());
        assert_eq!(
            data_paths(&i, &ids(&["steam", "youtube", "gone"])),
            [
                ".local/share/Steam",
                ".steam",
                ".local/share/hinteros/firefox/youtube"
            ]
        );
    }
}
