# User

This crate implements a thread-safe, cooperative interface for transitioning between the various Operating Modes in a `SetUID` executable. It support a greedy reentrancy, and uses a scoped object to guarantee a particular mode for its lifetime.

## Usage

Creating a `UserScope` object with a desired mode guarantees that mode will persist while the object remains alive.

```rust
use user::{UserScope, Mode};

{
    let _real_scope = UserScope::new(Mode::Real);
    // ... Do things with real.
    
    drop(_real_scope);
    // No guarantee on operating mode.
}
```

To avoid managing the object, you can also use the `as!` macros to wrap an expression or block that will be run as that mode.

```rust
use user::{Mode, as_real};

as_real!({
    println!("Hello from real!");
});

// No guarantees on operating mode.
```

## Thread Safety and Scheduling

The `resuid` and `resgid` of a program is process-wide. This means that calling `setresuid` from one thread will affect all other threads in the process. This library enforces thread-exclusive Operating Mode with a form of cooperative scheduling. 

When a thread initializes a new `UserScope`, the desired mode is enforced across all threads. Threads requesting another mode must wait until the current schedule has completed before they can request a new schedule. If a thread needs a mode that has already been scheduled by another, they are able to cooperatively participate without blocking the thread. Only once all cooperating threads have dropped their `UserScope`'s can another thread change the mode:

```rust
use user::as_real;
use std::thread;

for i in 0..50 {
    thread::spawn(|| {
        as_real!({
            println!("Hello! We can cooperate without blocking!");
        })
    });
}
```

## Greedy Reentrancy

The `UserScope`, as the name implies, is bound to a particular scope, not a thread; multiple scopes can exist in a single thread, including in nested scopes (Such as a scope defined in one function, then another defined in a child call). To implement this, `UserScope`'s use a greedy reentrancy. If a thread has exclusive control over the Operating Mode, it is free to change that mode without its previous scope dying. Other threads can participate in this new mode, with the primary thread creating a stack to manage its states and transitions.

```rust 
use user::{as_real, as_effective};
use std::thread;

for i in 0..50 {
    thread::spawn(|| {
        as_real!({
            println!("Hello! We can cooperate without blocking!");
            as_effective!({
                println!("Hello! This cannot be done without blocking, so the threads will take control of the state and cooperate if they can!")
            });
        })
    });
}
```
