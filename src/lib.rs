use log::{debug, error, info, trace, warn};
use std::boxed::Box;
use std::cmp::{Eq, Ord};
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::Send;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

#[derive(Debug)]
pub enum Error {
    InvalidState(String),
    NoHandler(String),
    DispatchFail(String),
}

pub trait StateHandlerContainer<
    T: Send + Sync + 'static + Debug,
    S: Hash + Eq,
    C: Sync + Send + 'static,
>
{
    fn init(&self, context: &mut C, prev_state: Option<&S>);
    fn handle_action(&self, context: &mut C, action: T) -> Option<S>;
    fn exit(&self, context: &mut C, next_state: Option<&S>);
}

pub trait StateHandler<T: Send + Sync + 'static + Debug, S: Hash + Eq, C: Sync + Send + 'static> {
    fn init(&self, prev_state: Option<&S>);
    fn handle_action(&self, action: T) -> Option<S>;
    fn exit(&self, next_state: Option<&S>);
}

pub struct StateMachine<
    A: Sync + Send + 'static + Debug,
    S: Sync + Send + 'static + Ord + Hash + Eq + Clone,
    C: Sync + Send + 'static,
    H: StateHandlerContainer<A, S, C> + Clone + Default + Sync + Send + 'static,
> {
    map: HashMap<S, Box<H>>,
    state: Arc<Mutex<S>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    action_queue: Arc<Mutex<Option<Sender<A>>>>,
    context: Arc<Mutex<C>>,
}

impl<A, S, C, H> StateMachine<A, S, C, H>
where
    H: StateHandlerContainer<A, S, C> + Clone + Default + Sync + Send + 'static,
    S: Sync + Send + 'static + Ord + Hash + Eq + Clone + Debug,
    C: Sync + Send + 'static,
    A: Send + Sync + 'static + Debug,
{
    pub fn new(context: &Arc<Mutex<C>>, handlers: Vec<(S, Box<H>)>) -> Self {
        let mut sorted_handlers = handlers.clone();
        sorted_handlers.sort_by(|a, b| a.0.cmp(&b.0));
        let (init_state, _) = sorted_handlers.get(0).unwrap();
        StateMachine {
            map: handlers.into_iter().collect::<HashMap<S, Box<H>>>(),
            state: Arc::new(Mutex::new(init_state.clone())),
            handle: Arc::new(Mutex::new(None)),
            action_queue: Arc::new(Mutex::new(None)),
            context: context.clone(),
        }
    }

    fn switch_state(
        context: &mut C,
        from_state: Option<&S>,
        to_state: Option<&S>,
        handlers: &HashMap<S, Box<H>>,
    ) -> Result<(), Error> {
        if let Some(state) = from_state {
            match handlers.get(state) {
                Some(handler) => Ok(handler.exit(context, to_state)),
                None => Err(Error::NoHandler(format!(
                    "no state handler for {:?}",
                    from_state
                ))),
            }?;
        }
        if let Some(state) = to_state {
            return match handlers.get(state) {
                Some(handler) => Ok(handler.init(context, from_state)),
                None => Err(Error::NoHandler(format!(
                    "no handler for state {:?}",
                    to_state
                ))),
            };
        }
        Ok(())
    }

    pub fn stop(self) {
        {
            let mut action_queue = self.action_queue.lock().unwrap();
            action_queue.take().map(drop);
        }
        {
            let mut handle = self.handle.lock().unwrap();
            handle.take().map(JoinHandle::join);
        }
    }

    pub fn start(&mut self) -> Result<(), Error> {
        let (tx, rx) = channel::<A>();
        if let Ok(mut queue) = self.action_queue.lock() {
            *queue = Some(tx);
        }
        let handlers = self.map.clone();
        let shared_state = self.state.clone();
        let shared_context = self.context.clone();
        if let Ok(mut handle) = self.handle.lock() {
            *handle = Some(thread::spawn(move || {
                {
                    let init_state = &*shared_state.lock().unwrap();
                    let mut context = shared_context.lock().unwrap();
                    if let Err(e) =
                        Self::switch_state(&mut *context, None, Some(init_state), &handlers)
                    {
                        warn!("error on initializing state machine : {:?}", e);
                    }
                }
                for action in rx {
                    let current_state = { &*shared_state.lock().unwrap() };
                    let next = match handlers.get(current_state) {
                        Some(handler) => {
                            let mut context = shared_context.lock().unwrap();
                            handler.handle_action(&mut *context, action)
                        }
                        _ => None,
                    };

                    if let Some(next_state) = next {
                        let mut context = shared_context.lock().unwrap();
                        if let Err(e) = Self::switch_state(
                            &mut *context,
                            Some(current_state),
                            Some(&next_state),
                            &handlers,
                        ) {
                            warn!("error on switching state : {:?}", e);
                        }
                    }
                }
                {
                    let exit_state = &*shared_state.lock().unwrap();
                    let mut context = shared_context.lock().unwrap();
                    if let Err(e) =
                        Self::switch_state(&mut *context, Some(exit_state), None, &handlers)
                    {
                        warn!("error on exit state machine : {:?}", e);
                    }
                }
            }));
        }
        Ok(())
    }

    pub fn get_state(&self) -> S {
        self.state.lock().unwrap().clone()
    }

    pub fn dispatch(&mut self, action: A) -> Result<(), Error> {
        {
            match &*self.action_queue.lock().unwrap() {
                Some(q) => match q.send(action) {
                    Ok(_) => Ok(()),
                    Err(e) => Err(Error::DispatchFail(format!(
                        "fail to dispatch action: {:?}",
                        e
                    ))),
                },
                _ => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::{Ordering, PartialOrd};
    use std::default::Default;

    #[derive(Hash, PartialEq, Clone, Debug)]
    enum TestState {
        Init = 0,
        StateOne = 1,
        StateTwo = 2,
    }

    impl PartialOrd for TestState {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            (self.clone() as u32).partial_cmp(&(other.clone() as u32))
        }
    }

    impl Eq for TestState {}

    impl Ord for TestState {
        fn cmp(&self, other: &Self) -> Ordering {
            (self.clone() as u32).cmp(&(other.clone() as u32))
        }
    }

    #[derive(Default, Clone)]
    struct TestStateHandler {
        age: Arc<Mutex<u32>>,
    }

    impl StateHandlerContainer<TestAction, TestState, TestHandle> for TestStateHandler {
        fn init(&self, context: &mut TestHandle, prev: Option<&TestState>) {}
        fn handle_action(&self, context: &mut TestHandle, action: TestAction) -> Option<TestState> {
            println!("action : {:?}", action);
            match &action {
                TestAction::Increase => {
                    context.set(context.get() + 1);
                }
                _ => {}
            }
            None
        }
        fn exit(&self, context: &mut TestHandle, next: Option<&TestState>) {}
    }

    struct TestHandle {
        val: u32,
    }

    impl TestHandle {
        pub fn set(&mut self, val: u32) {
            self.val = val;
        }

        pub fn get(&self) -> u32 {
            self.val
        }
    }

    #[derive(Debug)]
    enum TestAction {
        Increase,
        Decrease,
    }

    #[test]
    fn state_sorted() {
        let ctx = Arc::new(Mutex::new(TestHandle { val: 0 }));
        let mut sm = StateMachine::new(
            &ctx,
            vec![
                (TestState::StateOne, Box::new(TestStateHandler::default())),
                (TestState::Init, Box::new(TestStateHandler::default())),
            ],
        );
        sm.start().unwrap();
        for _ in 0..10 {
            sm.dispatch(TestAction::Increase).unwrap();
        }
        assert_eq!(sm.get_state(), TestState::Init);
        sm.stop();
        {
            let context = &*ctx.lock().unwrap();
            assert_eq!(context.get(), 10);
        }
    }
}
