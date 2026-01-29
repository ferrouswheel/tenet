# Event-Based Simulation Architecture

## Overview

This document outlines the plan to **replace** Tenet's simulation from a **step-based synchronous model** to an **event-based asynchronous model** with temporal ordering and configurable time control.

**Important**: This is a **complete replacement**, not a gradual migration. The step-based implementation will be entirely removed after the event-based system is validated. No backward compatibility will be maintained.

### Motivation

The current step-based simulation has several limitations:

1. **Fixed time granularity**: All events occur at step boundaries (e.g., every 5 minutes)
2. **Synchronous execution**: All actions in a step happen "simultaneously"
3. **No message latency**: Messages are either delivered instantly (direct) or at next poll (relay)
4. **Coarse temporal resolution**: Cannot model sub-step timing or realistic network delays
5. **Pre-planned sends only**: All messages scheduled upfront, no dynamic reactions

An event-based simulation will enable:

- **Continuous time**: Events at arbitrary timestamps (e.g., T=127.3 seconds)
- **Realistic latency**: Message send at T, delivery at T+latency
- **Temporal ordering**: Process events in chronological order regardless of creation order
- **Dynamic event generation**: Events can schedule future events
- **Flexible time control**: Fast-forward mode or real-time with speed multiplier
- **Event logging**: Complete audit trail for debugging and replay

## Current Architecture Summary

### Step-Based Execution Model

```rust
// Current: Fixed step loop
for step in 0..total_steps {
    // 1. Determine online status for this step
    // 2. Process all planned sends for this step
    // 3. Deliver messages if sender/recipient both online
    // 4. Detect online/offline transitions
    // 5. Forward stored messages to newly online peers
    // 6. Poll relay inbox for all online peers
    // 7. Announce online status to peers
    // 8. Process pending broadcasts
}
```

**Time Model:**
- `step` index (0, 1, 2, ...)
- `simulated_time = step * seconds_per_step * speed_factor`
- Fixed `seconds_per_step` (e.g., 300 seconds = 5 minutes)
- Speed factor multiplies progression rate

**Key Data Structures:**
- `HashMap<usize, Vec<SimMessage>>` - Messages indexed by step
- `Vec<bool>` - Per-client online schedule indexed by step
- `MetricsTracker` - Latency measured in steps, converted to seconds

### Message Flow

```
PlannedSend (step N)
  → SimMessage created
    → Envelope built
      → Direct delivery (if both online + direct link)
        OR
      → Post to relay
        → Recipient polls inbox (step N+1)
          → Received
```

**No explicit latency**: Delivery happens same-step (direct) or next-poll (relay)

### Random Event Generation

Events are pre-generated with step indices:

```rust
// Example: Poisson message frequency
let lambda_per_hour = 7.0;
let steps_per_hour = 3600.0 / seconds_per_step;
let lambda_per_step = lambda_per_hour / steps_per_hour;

for step in 0..total_steps {
    let count = sample_poisson(lambda_per_step, rng);
    for _ in 0..count {
        planned_sends.entry(step).or_insert(vec![]).push(...);
    }
}
```

## Event-Based Architecture Design

### Core Principles

1. **Continuous Time**: Use `f64` timestamps (seconds since simulation start)
2. **Priority Queue**: Process events in temporal order (not insertion order)
3. **Event Logging**: Append-only log of processed events (separate from pending queue)
4. **Latency Model**: Explicit delays between causally related events
5. **Time Control**: Support fast-forward and real-time modes
6. **Separation of Concerns**: Keep event system in `simulation/`, core library unchanged

### Event Queue

```rust
use std::collections::BinaryHeap;
use std::cmp::Ordering;

/// Event scheduled to occur at a specific simulated time
#[derive(Debug, Clone)]
pub struct ScheduledEvent {
    pub time: f64,           // Simulated seconds since start
    pub event: Event,        // The actual event data
}

impl Ord for ScheduledEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap (earliest time first)
        other.time.partial_cmp(&self.time)
            .unwrap_or(Ordering::Equal)
    }
}

/// Priority queue of pending events
pub struct EventQueue {
    heap: BinaryHeap<ScheduledEvent>,
    event_counter: u64,      // For tie-breaking and unique IDs
}

impl EventQueue {
    pub fn push(&mut self, time: f64, event: Event) { ... }
    pub fn pop(&mut self) -> Option<ScheduledEvent> { ... }
    pub fn peek_time(&self) -> Option<f64> { ... }
    pub fn is_empty(&self) -> bool { ... }
}
```

**Note**: Events are **not FIFO**. Multiple events at T=100.0 may be created at different times, but all are processed before any event at T=100.1.

### Event Types

```rust
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
        send_time: f64,        // For latency tracking
    },

    /// Client transitions between online/offline
    OnlineTransition {
        client_id: String,
        going_online: bool,    // true=online, false=offline
    },

    /// Client polls relay inbox (fetch messages)
    InboxPoll {
        client_id: String,
    },

    /// Client announces online status to peers
    OnlineAnnounce {
        client_id: String,
    },

    /// Generic scheduled action (for extensibility)
    CustomAction {
        action_id: String,
        data: HashMap<String, String>,
    },
}
```

### Event Log

```rust
/// Record of an event that has been processed
#[derive(Debug, Clone, Serialize)]
pub struct ProcessedEvent {
    pub event_id: u64,
    pub simulated_time: f64,
    pub wall_time: std::time::Instant,
    pub event: Event,
    pub outcome: EventOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub enum EventOutcome {
    MessageSent { envelope_id: String, posted_to_relay: bool },
    MessageDelivered { accepted: bool },
    OnlineTransitioned { new_state: bool },
    InboxPolled { messages_fetched: usize },
    // ... etc
}

pub struct EventLog {
    events: Vec<ProcessedEvent>,
    file: Option<std::fs::File>,  // Optional persistent log
}

impl EventLog {
    pub fn record(&mut self, event: ProcessedEvent) { ... }
    pub fn query(&self, filter: EventFilter) -> Vec<&ProcessedEvent> { ... }
    pub fn save_to_file(&self, path: &str) -> Result<()> { ... }
}
```

### Time Management

```rust
pub struct SimulationClock {
    /// Current simulated time (seconds)
    pub simulated_time: f64,

    /// Wall clock when simulation started
    start_instant: std::time::Instant,

    /// Last wall clock update
    last_update: std::time::Instant,

    /// Speed multiplier (sim seconds per real second)
    /// 1.0 = real-time, 2.0 = 2x speed, 0.0 = paused, f64::INFINITY = fast-forward
    pub speed_factor: f64,
}

pub enum TimeControlMode {
    /// Process events as fast as possible, jumping simulation time to next event
    FastForward,

    /// Advance simulation time based on real time elapsed
    /// speed_factor = simulated_seconds / real_seconds
    RealTime { speed_factor: f64 },

    /// Paused (speed_factor = 0.0)
    Paused,
}

impl SimulationClock {
    /// Update simulation time based on mode
    pub fn update(&mut self, mode: TimeControlMode) -> f64 {
        match mode {
            TimeControlMode::FastForward => {
                // Simulation time doesn't advance here;
                // caller will set it to next event time
                self.simulated_time
            }
            TimeControlMode::RealTime { speed_factor } => {
                let now = std::time::Instant::now();
                let real_elapsed = now.duration_since(self.last_update).as_secs_f64();
                let sim_elapsed = real_elapsed * speed_factor;
                self.last_update = now;
                self.simulated_time += sim_elapsed;
                self.simulated_time
            }
            TimeControlMode::Paused => {
                self.last_update = std::time::Instant::now();
                self.simulated_time
            }
        }
    }

    /// Jump simulation time to a specific value (for fast-forward)
    pub fn jump_to(&mut self, time: f64) {
        self.simulated_time = time;
        self.last_update = std::time::Instant::now();
    }
}
```

### Latency Model

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LatencyDistribution {
    /// Fixed latency (e.g., 0.5 seconds)
    Fixed { seconds: f64 },

    /// Uniform random latency
    Uniform { min: f64, max: f64 },

    /// Normal distribution (mean, std_dev)
    Normal { mean: f64, std_dev: f64 },

    /// Log-normal (for heavy-tailed delays)
    LogNormal { mean: f64, std_dev: f64 },
}

impl LatencyDistribution {
    pub fn sample(&self, rng: &mut impl rand::Rng) -> f64 { ... }
}

pub struct NetworkConditions {
    /// Latency for direct peer-to-peer delivery
    pub direct_latency: LatencyDistribution,

    /// Latency for posting to relay
    pub relay_post_latency: LatencyDistribution,

    /// Latency for fetching from relay
    pub relay_fetch_latency: LatencyDistribution,

    /// Probability of message drop (0.0 = no drops, 0.1 = 10% drop rate)
    pub drop_probability: f64,
}
```

**Message Send → Deliver Flow:**

```rust
// At time T, sender creates MessageSend event:
Event::MessageSend { sender_id, message }

// Event processed at time T:
//   1. Build envelope
//   2. Sample latency (e.g., 0.3 seconds)
//   3. Schedule MessageDeliver event at T + 0.3

EventQueue.push(T + 0.3, Event::MessageDeliver {
    recipient_id,
    envelope,
    send_time: T,
});

// At time T+0.3, MessageDeliver event processed:
//   1. Add to recipient's inbox
//   2. Record latency metric: 0.3 seconds
```

### Event-Based Random Generation

Convert from "events per step" to "events per time period" (normalized to 1 hour):

```rust
/// Generate message send events over a time window
pub fn generate_message_events(
    sender_id: &str,
    start_time: f64,
    end_time: f64,
    lambda_per_hour: f64,
    message_type_weights: &MessageTypeWeights,
    rng: &mut impl Rng,
) -> Vec<ScheduledEvent> {
    let duration_hours = (end_time - start_time) / 3600.0;
    let expected_count = lambda_per_hour * duration_hours;

    // Sample total count from Poisson distribution
    let count = sample_poisson(expected_count, rng);

    let mut events = Vec::new();
    for _ in 0..count {
        // Uniform distribution of event times within window
        let time = start_time + rng.gen::<f64>() * (end_time - start_time);

        // Sample message type
        let msg_type = message_type_weights.sample(rng.gen());

        events.push(ScheduledEvent {
            time,
            event: Event::MessageSend {
                sender_id: sender_id.to_string(),
                message: SimMessage { /* ... */ },
            },
        });
    }

    events
}
```

**Cohort-Based Online/Offline Events:**

```rust
/// Generate online/offline transition events for a cohort
pub fn generate_online_schedule_events(
    client_id: &str,
    cohort: &OnlineCohortDefinition,
    start_time: f64,
    end_time: f64,
    rng: &mut impl Rng,
) -> Vec<ScheduledEvent> {
    match cohort.cohort_type {
        CohortType::AlwaysOnline => {
            vec![ScheduledEvent {
                time: start_time,
                event: Event::OnlineTransition {
                    client_id: client_id.to_string(),
                    going_online: true,
                },
            }]
        }
        CohortType::RarelyOnline => {
            // Generate occasional online periods
            generate_session_events(client_id, start_time, end_time,
                                   sessions_per_day: 2.0,
                                   avg_duration_seconds: 300.0, rng)
        }
        CohortType::Diurnal { hourly_weights, timezone_offset, online_probability } => {
            // Sample online/offline transitions based on time-of-day
            generate_diurnal_events(client_id, start_time, end_time,
                                   hourly_weights, timezone_offset,
                                   online_probability, rng)
        }
    }
}
```

### Main Event Loop

```rust
pub struct EventBasedHarness {
    pub clock: SimulationClock,
    pub event_queue: EventQueue,
    pub event_log: EventLog,
    pub clients: HashMap<String, Box<dyn Client>>,
    pub network_conditions: NetworkConditions,
    pub time_control_mode: TimeControlMode,
    // ... other fields from SimulationHarness
}

impl EventBasedHarness {
    pub fn run(&mut self) -> SimulationReport {
        loop {
            // 1. Update simulation time based on mode
            match self.time_control_mode {
                TimeControlMode::FastForward => {
                    // Jump to next event time
                    if let Some(next_time) = self.event_queue.peek_time() {
                        self.clock.jump_to(next_time);
                    } else {
                        break; // No more events
                    }
                }
                TimeControlMode::RealTime { speed_factor } => {
                    // Advance based on real time
                    self.clock.update(self.time_control_mode);

                    // Wait if we're ahead of next event
                    if let Some(next_time) = self.event_queue.peek_time() {
                        if self.clock.simulated_time < next_time {
                            let wait = (next_time - self.clock.simulated_time) / speed_factor;
                            std::thread::sleep(Duration::from_secs_f64(wait));
                            continue;
                        }
                    } else {
                        break; // No more events
                    }
                }
                TimeControlMode::Paused => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }

            // 2. Process all events at current time
            while let Some(scheduled) = self.event_queue.peek() {
                if scheduled.time > self.clock.simulated_time {
                    break; // Future event, wait for it
                }

                let scheduled = self.event_queue.pop().unwrap();
                let outcome = self.process_event(scheduled.event);

                // 3. Record in event log
                self.event_log.record(ProcessedEvent {
                    event_id: scheduled.event_counter,
                    simulated_time: scheduled.time,
                    wall_time: std::time::Instant::now(),
                    event: scheduled.event,
                    outcome,
                });
            }

            // 4. Check stop conditions
            if self.should_stop() {
                break;
            }
        }

        self.generate_report()
    }

    fn process_event(&mut self, event: Event) -> EventOutcome {
        match event {
            Event::MessageSend { sender_id, message } => {
                self.handle_message_send(&sender_id, message)
            }
            Event::MessageDeliver { recipient_id, envelope, send_time } => {
                self.handle_message_deliver(&recipient_id, envelope, send_time)
            }
            Event::OnlineTransition { client_id, going_online } => {
                self.handle_online_transition(&client_id, going_online)
            }
            Event::InboxPoll { client_id } => {
                self.handle_inbox_poll(&client_id)
            }
            Event::OnlineAnnounce { client_id } => {
                self.handle_online_announce(&client_id)
            }
            Event::CustomAction { action_id, data } => {
                self.handle_custom_action(&action_id, data)
            }
        }
    }
}
```

## Phased Implementation Plan

**Note**: During Phases 1-5, both step-based and event-based implementations may coexist temporarily for validation purposes. However, **Phase 6 will completely remove the step-based code**. This is not a gradual migration—it's a complete replacement.

### Phase 1: Event Queue Infrastructure (1-2 days) ✅ COMPLETED

**Goal**: Establish event queue, event log, and time management without changing simulation behavior.

**Tasks**:
1. Create `src/simulation/event.rs` module
   - Define `Event` enum with all event types
   - Define `ScheduledEvent` with `time` and `event` fields
   - Implement `Ord` for min-heap ordering by time

2. Implement `EventQueue`
   - Wrapper around `BinaryHeap<ScheduledEvent>`
   - Methods: `push()`, `pop()`, `peek_time()`, `is_empty()`
   - Event counter for unique IDs and tie-breaking

3. Implement `EventLog`
   - `Vec<ProcessedEvent>` for in-memory storage
   - Optional file-based persistence (JSON lines format)
   - Query/filter methods for debugging

4. Implement `SimulationClock`
   - Track `simulated_time` as `f64`
   - Support `TimeControlMode` enum
   - Methods: `update()`, `jump_to()`

5. Add configuration types
   - `TimeControlMode` enum in `config.rs`
   - Extend `SimulationTimingConfig` with event-based fields

**Testing**:
- Unit tests for `EventQueue` ordering
- Unit tests for `SimulationClock` time progression
- Unit tests for `EventLog` record/query

**Acceptance Criteria**:
- Event queue correctly orders events by time
- Clock advances in both fast-forward and real-time modes
- Event log records and persists events

---

### Phase 2: Convert Message Delivery with Latency (2-3 days) ✅ COMPLETED

**Goal**: Split message sending into separate send/deliver events with configurable latency.

**Tasks**:
1. Add `NetworkConditions` struct
   - `LatencyDistribution` enum (Fixed, Uniform, Normal, LogNormal)
   - Implement `sample()` method using `rand` distributions
   - Add to `SimulationScenarioConfig` (optional, default to zero latency)

2. Refactor message sending in `EventBasedHarness`
   - `handle_message_send()`: Create envelope, sample latency, schedule `MessageDeliver`
   - `handle_message_deliver()`: Add to inbox, record latency metric
   - Update `ClientContext` to work with event times instead of steps

3. Convert `build_planned_sends()` to generate `MessageSend` events
   - Instead of `HashMap<step, Vec<SimMessage>>`, create `Vec<ScheduledEvent>`
   - Keep existing Poisson sampling, but assign continuous timestamps
   - Distribute events uniformly within time windows

4. Update metrics tracking
   - Change `message_send_steps: HashMap<String, usize>` to `message_send_times: HashMap<String, f64>`
   - Update latency calculations to use event times directly
   - Preserve existing metrics API for reporting

5. Refactor `SimulationHarness` to use event-based execution
   - Replace step-based loop with event loop
   - Rename to `EventBasedHarness` or update in-place
   - Keep step-based implementation temporarily for validation (will be removed in Phase 6)

**Testing**:
- Integration test: Send message with fixed latency, verify delivery time
- Integration test: Send message with uniform latency, verify range
- Compare outputs with step-based implementation for validation (zero latency)
- Verify metrics are accurate

**Acceptance Criteria**:
- Messages delivered with correct latency
- Metrics track send/deliver times accurately
- Event-based simulation produces sensible results

---

### Phase 3: Dynamic Online/Offline Transitions (2-3 days) ✅ COMPLETED

**Goal**: Generate online/offline transitions as events instead of pre-computed schedules.

**Tasks**:
1. Create `generate_online_schedule_events()` in `scenario.rs`
   - For `AlwaysOnline`: Single online event at T=0
   - For `RarelyOnline`: Sample session start/end times
   - For `Diurnal`: Sample transitions based on hourly weights and timezone
   - Return `Vec<ScheduledEvent>` with `OnlineTransition` events

2. Add `handle_online_transition()` to harness
   - Update client's online state
   - Trigger inbox poll if going online
   - Schedule online announcement to peers
   - Trigger store-and-forward delivery

3. Refactor `Client` trait for event-based state
   - Remove `schedule: Vec<bool>` from `SimulationClient`
   - Add `online_state: bool` field
   - Add `set_online(&mut self, online: bool)` method
   - Update `is_online()` to return current state (not step-based)

4. Update scenario builder
   - Replace `build_online_schedules()` with `build_online_events()`
   - Populate initial event queue with online/offline transitions

5. Add periodic inbox polling
   - When client goes online, schedule periodic `InboxPoll` events
   - Default: Poll every 60 seconds while online
   - Configurable via `SimulationConfig`

**Testing**:
- Unit test: AlwaysOnline cohort generates single online event
- Unit test: Diurnal cohort generates transitions at expected times
- Integration test: Client receives message after going online
- Integration test: Store-and-forward triggers on online transition

**Acceptance Criteria**:
- Online/offline state changes at correct times
- Clients poll inbox while online
- Store-and-forward delivers when recipient comes online
- Delivery rates are reasonable and consistent

---

### Phase 4: Time Control Modes (1-2 days)

**Goal**: Support fast-forward and real-time execution with speed control.

**Tasks**:
1. Implement `TimeControlMode` switching in event loop
   - Fast-forward: Jump to next event time immediately
   - Real-time: Sleep between events based on speed factor
   - Paused: No time progression, wait for resume

2. Add control commands
   - Extend `SimulationControlCommand` with:
     - `SetTimeControlMode { mode: TimeControlMode }`
     - `JumpToTime { time: f64 }` (for debugging)

3. Create TUI controls for event-based mode
   - Display current simulation time and speed
   - Hotkeys: `F` (fast-forward), `R` (real-time), `P` (pause)
   - Speed adjustment: `+`/`-` keys to change multiplier
   - Progress bar based on event queue depletion

4. Add time control to `tenet-sim` binary
   - CLI flags: `--speed-factor <f64>`, `--mode <fast|realtime|paused>`
   - Default to fast-forward for non-interactive runs
   - Default to real-time 1x for `--tui` mode

5. Update `run_with_progress_and_controls()`
   - Refactor to use event-based execution
   - Remove step-based loop logic

**Testing**:
- Unit test: Fast-forward processes all events immediately
- Unit test: Real-time 2x runs in half the wall clock time
- Integration test: Pause/resume preserves simulation state
- Manual test: TUI controls work as expected

**Acceptance Criteria**:
- Fast-forward mode runs at maximum speed (no sleeps)
- Real-time mode paces events according to speed factor
- Paused mode stops time progression without blocking UI
- Controls are responsive and intuitive

---

### Phase 5: Event-Based Random Generation (2-3 days)

**Goal**: Generate events dynamically with time-based distributions instead of step-based.

**Tasks**:
1. Add time-window event generation
   - `generate_message_events()`: Create message sends over time range
   - Accept `lambda_per_hour` instead of `lambda_per_step`
   - Uniform distribution of event times within window
   - Support other distributions (clustered, bursty)

2. Refactor `PostFrequency` config
   - Keep existing Poisson lambda_per_hour
   - Add optional time distribution: `Uniform`, `Clustered`, `Bursty`
   - Add time window configuration (e.g., "only during work hours")

3. Add session-based online generation for RarelyOnline
   - `generate_session_events()`: Sample session start times
   - Session duration from distribution (e.g., 5-15 minutes)
   - Create paired online/offline events

4. Improve diurnal pattern generation
   - `generate_diurnal_events()`: Use hourly weights to modulate transition probability
   - Sample transitions at finer granularity (e.g., every 15 minutes)
   - Apply timezone offset to weight vector

5. Support dynamic event generation (reactions)
   - When processing an event, allow scheduling new events
   - Example: Receiving a message triggers a reply after delay
   - Add `reaction_probability` and `reply_delay` to config

**Testing**:
- Unit test: Poisson 10 events/hour generates ~10 events in 1-hour window
- Unit test: Events uniformly distributed across time window
- Integration test: Diurnal cohort has higher online rate during peak hours
- Statistical test: Verify event distributions match configured rates

**Acceptance Criteria**:
- Message events normalized to lambda_per_hour
- Event times distributed according to configured distribution
- Online/offline patterns match cohort definitions
- Aggregate statistics match expected values

---

### Phase 6: Cleanup, Testing, Documentation (2-3 days)

**Goal**: Remove step-based simulation, validate event-based correctness, optimize performance, and document usage.

**Tasks**:
1. **Remove step-based simulation code**
   - Delete `SimulationHarness::run()` step-based loop (mod.rs:299-548)
   - Delete `build_online_schedules()` (scenario.rs)
   - Remove step-based fields from `SimulationClient`:
     - `schedule: Vec<bool>`
     - `is_online(step: usize)` - replace with `is_online() -> bool`
   - Remove step-indexed data structures:
     - `HashMap<usize, Vec<SimMessage>>` planned sends
     - `message_send_steps: HashMap<String, usize>`
   - Remove `SimulatedTimeConfig::seconds_per_step`
   - Keep only: `EventBasedHarness` and event-based APIs

2. **Delete obsolete tests**
   - Remove step-based integration tests:
     - `tests/simulation_harness_tests.rs` (step-based sections)
     - `tests/simulation_client_tests.rs` (step-based sections)
   - Remove step-based scenario fixtures (if any have hardcoded step logic)
   - Audit and remove any tests that assume fixed-step execution

3. **Create comprehensive event-based tests**
   - **Unit tests**:
     - Event queue ordering and tie-breaking
     - Latency distribution sampling
     - Time control mode switching
     - Event generation with Poisson/uniform distributions
   - **Integration tests** (`tests/event_based_simulation_tests.rs`):
     - Message send → deliver with latency
     - Online transition triggers inbox poll
     - Store-and-forward on peer online
     - Diurnal cohort online patterns
     - Fast-forward vs real-time execution
   - **Scenario-based tests**:
     - `scenarios/event_latency_test.toml`: Various latency distributions
     - `scenarios/event_burst_traffic.toml`: Clustered message generation
     - `scenarios/event_diurnal_patterns.toml`: Time-of-day online behavior
   - **Property tests**:
     - Event temporal ordering invariant
     - Message delivery latency >= configured minimum
     - Event log consistency (no gaps, monotonic time)

4. Performance optimization
   - Profile event loop to identify bottlenecks
   - Optimize event queue operations (consider `BTreeMap` for ties)
   - Batch event processing when multiple at same time
   - Memory optimization: Reuse allocations, shrink vectors

5. Event log analysis tools
   - `analyze_event_log.rs`: CLI tool to query logs
   - Filters: By event type, time range, client ID
   - Aggregations: Event frequency, latency histograms
   - Visualizations: Timeline view, causality graphs

6. Documentation
   - Update `docs/architecture.md` with event-based design
   - Create `docs/SIMULATION_GUIDE.md` for scenario authoring
   - Add inline documentation to all event types and APIs
   - Example scenario configs with comments explaining event-based parameters

**Testing**:
- All new event-based tests pass
- Scenarios demonstrate event features work correctly
- Performance benchmarks show acceptable speed
- Memory usage profiling shows no leaks
- Event log correctly records all processed events

**Acceptance Criteria**:
- Step-based simulation code completely removed
- All tests use event-based harness
- Documentation covers all event-based features
- No mentions of "steps" remain in simulation code (except in historical context)
- Performance is acceptable for typical scenarios

---

## Replacement Strategy

### Complete Replacement (No Backward Compatibility)

This is a **clean replacement** of the step-based simulation with event-based execution. The step-based implementation will be **completely removed** in Phase 6.

**Rationale**:
- Simplifies codebase (single implementation to maintain)
- Avoids confusion between two execution models
- Forces proper migration of all scenarios and tests
- Event-based model is strictly more capable

### Scenario Conversion

All existing scenario TOML files must be updated to work with event-based execution:

**Changes to configuration**:
1. Remove `steps` field (replaced by simulation duration)
2. Add `duration_seconds` or `max_time_seconds`
3. Update `simulated_time` config to remove `seconds_per_step`
4. Add optional `network_conditions` for latency

**Example conversion**:

```toml
# OLD (step-based):
[simulation]
node_ids = ["peer-1", "peer-2"]
steps = 60  # 60 steps

[simulation.simulated_time]
seconds_per_step = 300  # 5 minutes per step
default_speed_factor = 1.0

# NEW (event-based):
[simulation]
node_ids = ["peer-1", "peer-2"]
duration_seconds = 18000  # 5 hours (60 steps * 300 seconds)

[simulation.time_control]
mode = "fast-forward"  # Or "realtime" with speed_factor
speed_factor = 1.0

[simulation.network_conditions]
direct_latency = { type = "fixed", seconds = 0.1 }
relay_post_latency = { type = "uniform", min = 0.2, max = 0.5 }
relay_fetch_latency = { type = "fixed", seconds = 0.1 }
drop_probability = 0.0  # No message drops
```

### Testing Strategy

1. **During implementation (Phases 1-5)**: Both implementations exist temporarily
   - New tests use event-based harness
   - Keep existing tests running with step-based (to ensure no regressions)
   - Compare outputs between implementations for validation

2. **Phase 6 cleanup**:
   - Delete all step-based code
   - Remove obsolete tests
   - Update all scenario fixtures
   - Create comprehensive event-based test suite

3. **Final state**:
   - Only event-based implementation exists
   - All tests use event-based harness
   - All scenarios use event-based configuration

---

## Implementation Timeline

| Phase | Duration | Dependencies |
|-------|----------|--------------|
| Phase 1: Event Infrastructure | 1-2 days | None |
| Phase 2: Message Latency | 2-3 days | Phase 1 |
| Phase 3: Online/Offline Events | 2-3 days | Phase 1, 2 |
| Phase 4: Time Control | 1-2 days | Phase 1, 2, 3 |
| Phase 5: Event Generation | 2-3 days | Phase 1, 2, 3 |
| Phase 6: Testing & Docs | 2-3 days | Phase 1-5 |

**Total**: 10-16 days (2-3 weeks)

**Critical Path**: Phase 1 → Phase 2 → Phase 3 → Phase 4 (time control depends on full event system)

---

## Key Design Decisions

### 1. Why `f64` for time?

- **Continuous time**: Avoid quantization errors
- **Subsecond precision**: Model realistic network delays (100ms)
- **Sufficient range**: `f64` can represent years of simulation time with microsecond precision
- **Simple arithmetic**: Easy to add/subtract durations

### 2. Why separate send/deliver events?

- **Explicit latency**: Models real-world network delays
- **Causality tracking**: Send time preserved for metrics
- **Failure simulation**: Can drop messages between send/deliver
- **Realistic ordering**: Messages sent later can arrive earlier (if latency varies)

### 3. Why log processed events separately from queue?

- **Causality**: A processed event may schedule an event earlier than queued events
- **FIFO log**: Processed events are logged in temporal order, but not queue insertion order
- **Debugging**: Complete history of what happened, not what's pending
- **Replay**: Can reconstruct simulation from log

### 4. Why support multiple time control modes?

- **Development**: Fast-forward for quick iteration
- **Debugging**: Real-time with speed control to observe behavior
- **Demos**: Real-time 1x to show users realistic system behavior
- **Testing**: Pause/step for manual inspection

---

## Benefits of Event-Based Design

1. **Realism**: Models actual network latency and timing
2. **Flexibility**: Events can be generated dynamically based on system state
3. **Debuggability**: Complete event log shows exactly what happened when
4. **Scalability**: Continuous time allows arbitrary temporal resolution
5. **Extensibility**: Easy to add new event types (e.g., network partitions, peer churn)
6. **Control**: Fast-forward for development, real-time for demos
7. **Accuracy**: Metrics reflect true temporal behavior, not step-based approximations

---

## Future Enhancements (Post-Phase 6)

1. **Event replay**: Load event log and replay simulation exactly
2. **Distributed simulation**: Events processed by multiple workers
3. **Live relay integration**: Mix real HTTP relay with simulated clients
4. **Network partitions**: Model temporary disconnections between peers
5. **Adaptive latency**: Latency varies based on network congestion
6. **Event causality graph**: Visualize which events triggered which
7. **Snapshot/restore**: Save simulation state and resume later
8. **Event-driven scenarios**: Scenarios that react to simulation state (e.g., "add peer when 100 messages sent")

---

## References

### Relevant Files

- **Core simulation**: `src/simulation/mod.rs`, `src/simulation/scenario.rs`
- **Configuration**: `src/simulation/config.rs`
- **Metrics**: `src/simulation/metrics.rs`
- **Random generation**: `src/simulation/random.rs`
- **Client trait**: `src/client.rs` (lines 1106-1153, 1156-1170)

### Key Types

- `SimulationHarness` (mod.rs:72-90): Current step-based harness
- `SimulationClient` (client.rs:1156-1170): Client state and methods
- `SimulationConfig` (config.rs:88-117): Scenario configuration
- `SimulationMetrics` (metrics.rs:10-19): Aggregated metrics

### External Resources

- [Discrete Event Simulation](https://en.wikipedia.org/wiki/Discrete-event_simulation)
- [Priority Queue (Rust)](https://doc.rust-lang.org/std/collections/struct.BinaryHeap.html)
- [Poisson Process](https://en.wikipedia.org/wiki/Poisson_point_process)
