use super::*;

#[tokio::test]
async fn paused_until_every_registration_drops() {
    let service = ElicitationService::new();
    let paused = service.subscribe();
    assert!(!*paused.borrow());

    let first = service.register();
    let second = service.register();
    assert!(*paused.borrow());

    drop(first);
    assert!(*paused.borrow());

    drop(second);
    assert!(!*paused.borrow());
}
