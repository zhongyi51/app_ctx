#[derive(Clone)]
pub enum EventType {}

#[derive(Clone)]
pub struct AppEvent {
    event_type: EventType,
}
