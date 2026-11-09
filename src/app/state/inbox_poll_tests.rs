use super::inbox_new_mail_count;

#[test]
fn only_a_completed_poll_against_a_known_inbox_triggers_alerts() {
    assert_eq!(inbox_new_mail_count(true, true, 10, 12), 2);
    assert_eq!(inbox_new_mail_count(true, true, 10, 9), 0);
    assert_eq!(inbox_new_mail_count(true, false, 0, 12), 0);
    assert_eq!(inbox_new_mail_count(false, true, 10, 12), 0);
}
