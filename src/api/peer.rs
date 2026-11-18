//! Server-only connection evidence. Never deserialize identity from socket JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PeerProcess {
    ancestry: Vec<u32>,
    complete: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Caller<T> {
    Pane(T),
    External,
    Unavailable,
}

impl PeerProcess {
    pub(crate) fn capture(pid: Option<u32>) -> Self {
        Self::with_resolver(pid, crate::platform::parent_process_id)
    }

    fn with_resolver(pid: Option<u32>, mut parent: impl FnMut(u32) -> Option<u32>) -> Self {
        let mut evidence = Self::default();
        let Some(mut pid) = pid.filter(|pid| *pid > 0) else {
            return evidence;
        };
        // Bound cycles, corrupt process data and pathological process trees.
        for _ in 0..256 {
            if evidence.ancestry.contains(&pid) {
                break;
            }
            evidence.ancestry.push(pid);
            if pid == 1 {
                evidence.complete = true;
                break;
            }
            let Some(ppid) = parent(pid).filter(|pid| *pid > 0) else {
                break;
            };
            pid = ppid;
        }
        evidence
    }

    pub(crate) fn resolve<T: Clone + Eq>(
        &self,
        anchors: impl IntoIterator<Item = (T, Vec<u32>)>,
    ) -> Caller<T> {
        let anchors: Vec<_> = anchors.into_iter().collect();
        // Nearest ancestor wins, including a nested pane's own PTY child.
        for pid in &self.ancestry {
            let mut matches = anchors.iter().filter(|(_, pids)| pids.contains(pid));
            if let Some((pane, _)) = matches.next() {
                if matches.any(|(other, _)| other != pane) {
                    return Caller::Unavailable;
                }
                return Caller::Pane(pane.clone());
            }
        }
        if self.complete {
            Caller::External
        } else {
            Caller::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestry_resolves_descendants_and_the_nearest_pane() {
        let peer = PeerProcess::with_resolver(Some(50), |pid| match pid {
            50 => Some(40),
            40 => Some(30),
            30 => Some(1),
            _ => None,
        });
        assert_eq!(
            peer.resolve([(2, vec![30]), (1, vec![40])]),
            Caller::Pane(1)
        );
        assert_eq!(peer.resolve([(2, vec![30])]), Caller::Pane(2));
        assert_eq!(peer.resolve([(2, vec![90])]), Caller::External);
    }

    #[test]
    fn missing_credentials_incomplete_lookup_cycles_and_ambiguous_anchors_fail_closed() {
        for peer in [
            PeerProcess::with_resolver(None, |_| Some(1)),
            PeerProcess::with_resolver(Some(0), |_| Some(1)),
            PeerProcess::with_resolver(Some(50), |_| None),
            PeerProcess::with_resolver(Some(50), |_| Some(50)),
            PeerProcess::with_resolver(Some(500), |pid| Some(pid - 1)),
        ] {
            assert_eq!(peer.resolve([(1, vec![10])]), Caller::Unavailable);
        }
        let peer = PeerProcess::with_resolver(Some(50), |_| None);
        assert_eq!(peer.resolve([(1, vec![50])]), Caller::Pane(1));
        assert_eq!(
            peer.resolve([(1, vec![50]), (2, vec![50])]),
            Caller::Unavailable
        );
    }

    #[test]
    fn wire_claims_cannot_supply_server_evidence() {
        let params: crate::api::schema::AgentCloseParams = serde_json::from_str(
            r#"{"peer_process":{"ancestry":[1],"complete":true},"apply":true}"#,
        )
        .unwrap();
        assert_eq!(params.peer_process, PeerProcess::default());
        assert!(serde_json::to_value(params)
            .unwrap()
            .get("peer_process")
            .is_none());
    }
}
