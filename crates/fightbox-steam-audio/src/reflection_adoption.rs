pub(crate) struct ReflectionAdoption {
    next_source: usize,
}

impl ReflectionAdoption {
    pub(crate) fn new() -> Self {
        Self { next_source: 0 }
    }

    pub(crate) fn select(&mut self, pending: &[bool]) -> Option<usize> {
        if pending.is_empty() {
            return None;
        }
        let start = self.next_source % pending.len();
        for offset in 0..pending.len() {
            let index = (start + offset) % pending.len();
            if pending[index] {
                self.next_source = (index + 1) % pending.len();
                return Some(index);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_pending_sources_are_fair_and_wait_at_most_seven_blocks() {
        let mut adoption = ReflectionAdoption::new();
        let pending = [true; 8];
        let mut last_selected = [None; 8];
        for block in 0..24 {
            let selected = adoption.select(&pending).unwrap();
            assert_eq!(selected, block % pending.len());
            if let Some(previous) = last_selected[selected] {
                assert_eq!(block - previous - 1, 7);
            } else {
                assert!(block <= 7);
            }
            last_selected[selected] = Some(block);
        }
        assert!(last_selected.into_iter().all(|selected| selected.is_some()));
    }

    #[test]
    fn skips_ineligible_and_coalesced_slots_without_moving_on_none() {
        let mut adoption = ReflectionAdoption::new();
        let mut pending = [false, true, false, false, true, false, false, true];
        assert_eq!(adoption.select(&pending), Some(1));
        pending[1] = false;
        assert_eq!(adoption.select(&pending), Some(4));
        pending[4] = false;
        assert_eq!(adoption.select(&pending), Some(7));
        assert_eq!(adoption.select(&[false; 8]), None);

        pending = [false, false, true, false, false, false, true, false];
        assert_eq!(adoption.select(&pending), Some(2));
        assert_eq!(adoption.select(&[false; 8]), None);
        assert_eq!(adoption.select(&[]), None);
        assert_eq!(adoption.select(&pending), Some(6));
        pending[6] = false;
        assert_eq!(adoption.select(&pending), Some(2));
        assert_eq!(adoption.select(&pending), Some(2));
    }
}
