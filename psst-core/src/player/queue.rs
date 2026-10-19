use rand::prelude::SliceRandom;

use super::PlaybackItem;

#[derive(Default, Debug)]
pub enum QueueBehavior {
    #[default]
    Sequential,
    Random,
    LoopTrack,
    LoopAll,
}

pub struct Queue {
    items: Vec<PlaybackItem>,
    user_items: Vec<PlaybackItem>,
    position: usize,
    user_items_position: usize,
    positions: Vec<usize>,
    behavior: QueueBehavior,
}

impl Queue {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            user_items: Vec::new(),
            position: 0,
            user_items_position: 0,
            positions: Vec::new(),
            behavior: QueueBehavior::default(),
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.positions.clear();
        self.position = 0;
    }

    pub fn fill(&mut self, items: Vec<PlaybackItem>, position: usize) {
        self.positions.clear();
        self.items = items;
        self.position = position;
        self.compute_positions();
    }

    pub fn add(&mut self, item: PlaybackItem) {
        self.user_items.push(item);
    }

    fn handle_added_queue(&mut self) {
        if self.user_items.len() > self.user_items_position {
            self.items.insert(
                self.positions.len(),
                self.user_items[self.user_items_position],
            );
            self.positions
                .insert(self.position + 1, self.positions.len());
            self.user_items_position += 1;
        }
    }

    pub fn set_behaviour(&mut self, behavior: QueueBehavior) {
        self.behavior = behavior;
        self.compute_positions();
    }

    fn compute_positions(&mut self) {
        // In the case of switching away from shuffle, the position should be set back to
        // where it appears in the actual playlist order.
        let playlist_position = if self.positions.len() > 1 {
            self.positions[self.position]
        } else {
            self.position
        };
        // Start with an ordered 1:1 mapping.
        self.positions = (0..self.items.len()).collect();

        if let QueueBehavior::Random = self.behavior {
            // Swap the current position with the first item, so we will start from the
            // beginning, with the full queue ahead of us.  Then shuffle the rest of the
            // items and set the position to 0.
            if self.positions.len() > 1 {
                self.positions.swap(0, self.position);
                self.positions[1..].shuffle(&mut rand::rng());
            }
            self.position = 0;
        } else {
            self.position = playlist_position;
        }
    }

    pub fn skip_to_previous(&mut self) {
        self.position = self.previous_position();
    }

    pub fn skip_to_next(&mut self) {
        self.handle_added_queue();
        self.position = self.next_position();
    }

    pub fn skip_to_following(&mut self) {
        self.handle_added_queue();
        self.position = self.following_position();
    }

    pub fn get_current(&self) -> Option<&PlaybackItem> {
        let position = self.positions.get(self.position).copied()?;
        self.items.get(position)
    }

    /// The item that plays after the current one ends, for preloading.
    pub fn get_following(&self) -> Option<&PlaybackItem> {
        // Items added by the user go first.
        if let Some(item) = self.user_items.get(self.user_items_position) {
            return Some(item);
        }
        if self.items.is_empty() {
            return None;
        }
        let position = self.positions.get(self.following_position()).copied()?;
        self.items.get(position)
    }

    fn previous_position(&self) -> usize {
        match self.behavior {
            QueueBehavior::Sequential
            | QueueBehavior::Random
            | QueueBehavior::LoopTrack
            | QueueBehavior::LoopAll => self.position.saturating_sub(1),
        }
    }

    fn next_position(&self) -> usize {
        match self.behavior {
            QueueBehavior::Sequential | QueueBehavior::Random | QueueBehavior::LoopTrack => {
                self.position + 1
            }
            QueueBehavior::LoopAll => (self.position + 1) % self.items.len(),
        }
    }

    fn following_position(&self) -> usize {
        match self.behavior {
            QueueBehavior::Sequential | QueueBehavior::Random => self.position + 1,
            QueueBehavior::LoopTrack => self.position,
            QueueBehavior::LoopAll => (self.position + 1) % self.items.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{audio::normalize::NormalizationLevel, item_id::ItemId};

    fn items(count: u128) -> Vec<PlaybackItem> {
        (1..=count)
            .map(|id| PlaybackItem {
                item_id: ItemId::new(id, crate::item_id::ItemIdType::Track),
                norm_level: NormalizationLevel::Track,
            })
            .collect()
    }

    fn current(queue: &Queue) -> u128 {
        queue.get_current().unwrap().item_id.id
    }

    #[test]
    fn following_is_the_next_track() {
        let mut queue = Queue::new();
        queue.fill(items(5), 2);
        assert_eq!(current(&queue), 3);
        assert_eq!(queue.get_following().unwrap().item_id.id, 4);
        queue.skip_to_following();
        assert_eq!(current(&queue), 4);
    }

    #[test]
    fn shuffle_starts_with_the_clicked_track_and_preloads_the_real_next() {
        let mut queue = Queue::new();
        queue.set_behaviour(QueueBehavior::Random);
        for clicked in 0..10 {
            queue.fill(items(10), clicked);
            assert_eq!(current(&queue), clicked as u128 + 1);
            let following = queue.get_following().unwrap().item_id.id;
            assert_ne!(following, current(&queue));
            queue.skip_to_following();
            assert_eq!(current(&queue), following);
        }
    }

    #[test]
    fn added_tracks_come_next() {
        let mut queue = Queue::new();
        queue.fill(items(3), 0);
        queue.add(items(9)[8]);
        assert_eq!(queue.get_following().unwrap().item_id.id, 9);
        queue.skip_to_following();
        assert_eq!(current(&queue), 9);
        assert_eq!(queue.get_following().unwrap().item_id.id, 2);
    }

    #[test]
    fn loop_track_repeats_and_end_of_queue_stops() {
        let mut queue = Queue::new();
        queue.fill(items(2), 1);
        assert!(queue.get_following().is_none());
        queue.set_behaviour(QueueBehavior::LoopTrack);
        assert_eq!(queue.get_following().unwrap().item_id.id, 2);
    }
}
