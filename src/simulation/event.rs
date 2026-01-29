use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::protocol::Envelope;

use super::scenario::SimMessage;

/// Event types that can occur during simulation
#[derive(Debug, Clone)]
pub enum Event {
    /// A client sends a message (creates envelope, posts to relay or direct)
    MessageSend {
        sender_id: String,
        message: SimMessage,
    },

    /// A message is delivered to recipient (arrives in inbox)
    MessageDeliver {
        recipient_id: String,
        envelope: Envelope,
        send_time: f64, // For latency tracking
    },

    /// Client transitions between online/offline
    OnlineTransition {
        client_id: String,
        going_online: bool, // true=online, false=offline
    },

    /// Client polls relay inbox (fetch messages)
    InboxPoll { client_id: String },

    /// Client announces online status to peers
    OnlineAnnounce { client_id: String },

    /// Generic scheduled action (for extensibility)
    CustomAction {
        action_id: String,
        data: HashMap<String, String>,
    },
}

/// Event scheduled to occur at a specific simulated time
#[derive(Debug, Clone)]
pub struct ScheduledEvent {
    pub time: f64,     // Simulated seconds since start
    pub event: Event,  // The actual event data
    pub event_id: u64, // Unique identifier for this event
}

impl PartialEq for ScheduledEvent {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.event_id == other.event_id
    }
}

impl Eq for ScheduledEvent {}

impl PartialOrd for ScheduledEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap (earliest time first)
        // If times are equal, use event_id for deterministic tie-breaking
        match other.time.partial_cmp(&self.time) {
            Some(Ordering::Equal) => other.event_id.cmp(&self.event_id),
            Some(ord) => ord,
            None => {
                // Handle NaN by treating it as greater than any finite value
                if self.time.is_nan() {
                    if other.time.is_nan() {
                        Ordering::Equal
                    } else {
                        Ordering::Greater
                    }
                } else {
                    Ordering::Less
                }
            }
        }
    }
}

/// Priority queue of pending events
pub struct EventQueue {
    heap: BinaryHeap<ScheduledEvent>,
    event_counter: u64, // For unique IDs and tie-breaking
}

impl EventQueue {
    pub fn new() -> Self {
        EventQueue {
            heap: BinaryHeap::new(),
            event_counter: 0,
        }
    }

    /// Add an event to the queue at the specified time
    pub fn push(&mut self, time: f64, event: Event) {
        let event_id = self.event_counter;
        self.event_counter += 1;

        self.heap.push(ScheduledEvent {
            time,
            event,
            event_id,
        });
    }

    /// Remove and return the next event (earliest time)
    pub fn pop(&mut self) -> Option<ScheduledEvent> {
        self.heap.pop()
    }

    /// Get the time of the next event without removing it
    pub fn peek_time(&self) -> Option<f64> {
        self.heap.peek().map(|e| e.time)
    }

    /// Check if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Get the number of pending events
    pub fn len(&self) -> usize {
        self.heap.len()
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of processing an event
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventOutcome {
    MessageSent {
        envelope_id: String,
        posted_to_relay: bool,
    },
    MessageDelivered {
        accepted: bool,
    },
    OnlineTransitioned {
        new_state: bool,
    },
    InboxPolled {
        messages_fetched: usize,
    },
    OnlineAnnounced,
    CustomActionExecuted {
        success: bool,
    },
}

/// Record of an event that has been processed
#[derive(Debug, Clone, Serialize)]
pub struct ProcessedEvent {
    pub event_id: u64,
    pub simulated_time: f64,
    pub wall_time_millis: u64, // Milliseconds since simulation start
    pub outcome: EventOutcome,
}

/// Log of processed events
pub struct EventLog {
    events: Vec<ProcessedEvent>,
    start_instant: Instant,
    #[allow(dead_code)]
    file: Option<std::fs::File>, // Optional persistent log (for future use)
}

impl EventLog {
    pub fn new() -> Self {
        EventLog {
            events: Vec::new(),
            start_instant: Instant::now(),
            file: None,
        }
    }

    /// Record a processed event
    pub fn record(
        &mut self,
        event_id: u64,
        simulated_time: f64,
        wall_time: Instant,
        outcome: EventOutcome,
    ) {
        let wall_time_millis = wall_time.duration_since(self.start_instant).as_millis() as u64;

        self.events.push(ProcessedEvent {
            event_id,
            simulated_time,
            wall_time_millis,
            outcome,
        });
    }

    /// Get all recorded events
    pub fn events(&self) -> &[ProcessedEvent] {
        &self.events
    }

    /// Get the number of recorded events
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Check if the log is empty
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Query events by simulated time range
    pub fn events_in_time_range(&self, start: f64, end: f64) -> Vec<&ProcessedEvent> {
        self.events
            .iter()
            .filter(|e| e.simulated_time >= start && e.simulated_time <= end)
            .collect()
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Time control modes for the simulation
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimeControlMode {
    /// Process events as fast as possible, jumping simulation time to next event
    #[default]
    FastForward,

    /// Advance simulation time based on real time elapsed
    /// speed_factor = simulated_seconds / real_seconds
    RealTime { speed_factor: f64 },

    /// Paused (speed_factor = 0.0)
    Paused,
}

/// Clock for managing simulation time
pub struct SimulationClock {
    /// Current simulated time (seconds)
    pub simulated_time: f64,

    /// Wall clock when simulation started
    start_instant: Instant,

    /// Last wall clock update
    last_update: Instant,

    /// Current time control mode
    pub mode: TimeControlMode,
}

impl SimulationClock {
    pub fn new(mode: TimeControlMode) -> Self {
        let now = Instant::now();
        SimulationClock {
            simulated_time: 0.0,
            start_instant: now,
            last_update: now,
            mode,
        }
    }

    /// Update simulation time based on current mode
    /// Returns the updated simulation time
    pub fn update(&mut self) -> f64 {
        match self.mode {
            TimeControlMode::FastForward => {
                // Simulation time doesn't advance here;
                // caller will set it to next event time using jump_to()
                self.simulated_time
            }
            TimeControlMode::RealTime { speed_factor } => {
                let now = Instant::now();
                let real_elapsed = now.duration_since(self.last_update).as_secs_f64();
                let sim_elapsed = real_elapsed * speed_factor;
                self.last_update = now;
                self.simulated_time += sim_elapsed;
                self.simulated_time
            }
            TimeControlMode::Paused => {
                self.last_update = Instant::now();
                self.simulated_time
            }
        }
    }

    /// Jump simulation time to a specific value (for fast-forward mode)
    pub fn jump_to(&mut self, time: f64) {
        self.simulated_time = time;
        self.last_update = Instant::now();
    }

    /// Set the time control mode
    pub fn set_mode(&mut self, mode: TimeControlMode) {
        self.mode = mode;
        self.last_update = Instant::now();
    }

    /// Get elapsed wall time since simulation start
    pub fn wall_time_elapsed(&self) -> Duration {
        Instant::now().duration_since(self.start_instant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_queue_ordering() {
        let mut queue = EventQueue::new();

        // Add events in non-chronological order
        queue.push(
            10.0,
            Event::InboxPoll {
                client_id: "a".to_string(),
            },
        );
        queue.push(
            5.0,
            Event::InboxPoll {
                client_id: "b".to_string(),
            },
        );
        queue.push(
            15.0,
            Event::InboxPoll {
                client_id: "c".to_string(),
            },
        );
        queue.push(
            5.0,
            Event::InboxPoll {
                client_id: "d".to_string(),
            },
        );

        // Events should be processed in chronological order
        assert_eq!(queue.len(), 4);

        let e1 = queue.pop().unwrap();
        assert_eq!(e1.time, 5.0);

        let e2 = queue.pop().unwrap();
        assert_eq!(e2.time, 5.0);

        let e3 = queue.pop().unwrap();
        assert_eq!(e3.time, 10.0);

        let e4 = queue.pop().unwrap();
        assert_eq!(e4.time, 15.0);

        assert!(queue.is_empty());
    }

    #[test]
    fn test_event_queue_peek_time() {
        let mut queue = EventQueue::new();
        assert_eq!(queue.peek_time(), None);

        queue.push(
            10.0,
            Event::InboxPoll {
                client_id: "a".to_string(),
            },
        );
        assert_eq!(queue.peek_time(), Some(10.0));

        queue.push(
            5.0,
            Event::InboxPoll {
                client_id: "b".to_string(),
            },
        );
        assert_eq!(queue.peek_time(), Some(5.0));

        queue.pop();
        assert_eq!(queue.peek_time(), Some(10.0));

        queue.pop();
        assert_eq!(queue.peek_time(), None);
    }

    #[test]
    fn test_simulation_clock_fast_forward() {
        let mut clock = SimulationClock::new(TimeControlMode::FastForward);
        assert_eq!(clock.simulated_time, 0.0);

        clock.jump_to(10.0);
        assert_eq!(clock.simulated_time, 10.0);

        clock.jump_to(15.5);
        assert_eq!(clock.simulated_time, 15.5);
    }

    #[test]
    fn test_simulation_clock_real_time() {
        let mut clock = SimulationClock::new(TimeControlMode::RealTime { speed_factor: 2.0 });
        assert_eq!(clock.simulated_time, 0.0);

        // Sleep for a short time and update
        std::thread::sleep(Duration::from_millis(100));
        let time = clock.update();

        // Should advance approximately 0.2 seconds (100ms * 2.0 speed factor)
        assert!(time > 0.15 && time < 0.25, "time was {}", time);
    }

    #[test]
    fn test_simulation_clock_paused() {
        let mut clock = SimulationClock::new(TimeControlMode::Paused);
        assert_eq!(clock.simulated_time, 0.0);

        std::thread::sleep(Duration::from_millis(50));
        let time = clock.update();
        assert_eq!(time, 0.0);

        // Can still manually jump time while paused
        clock.jump_to(100.0);
        assert_eq!(clock.simulated_time, 100.0);
    }

    #[test]
    fn test_event_log_recording() {
        let mut log = EventLog::new();
        assert!(log.is_empty());

        let now = Instant::now();
        log.record(
            0,
            1.0,
            now,
            EventOutcome::MessageSent {
                envelope_id: "msg1".to_string(),
                posted_to_relay: true,
            },
        );

        assert_eq!(log.len(), 1);
        assert!(!log.is_empty());

        log.record(
            1,
            2.0,
            now,
            EventOutcome::MessageDelivered { accepted: true },
        );

        assert_eq!(log.len(), 2);
    }

    #[test]
    fn test_event_log_time_range_query() {
        let mut log = EventLog::new();
        let now = Instant::now();

        log.record(
            0,
            1.0,
            now,
            EventOutcome::MessageSent {
                envelope_id: "msg1".to_string(),
                posted_to_relay: true,
            },
        );
        log.record(
            1,
            5.0,
            now,
            EventOutcome::MessageDelivered { accepted: true },
        );
        log.record(
            2,
            10.0,
            now,
            EventOutcome::InboxPolled {
                messages_fetched: 3,
            },
        );

        let events = log.events_in_time_range(0.0, 6.0);
        assert_eq!(events.len(), 2);

        let events = log.events_in_time_range(5.0, 10.0);
        assert_eq!(events.len(), 2);

        let events = log.events_in_time_range(11.0, 20.0);
        assert_eq!(events.len(), 0);
    }
}
