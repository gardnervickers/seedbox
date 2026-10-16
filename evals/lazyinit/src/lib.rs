//! Worker threads lazily initialise shared state. The planted bug checks and
//! sets without a lock, so two threads that both see it uninitialised both
//! build it. Initialising takes long enough for one thread to finish before
//! the next runs, unless the scheduler switches threads inside it.
pub const PORT: u16 = 7006;
