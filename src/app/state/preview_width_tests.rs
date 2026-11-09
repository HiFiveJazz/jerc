use super::preview_percent_at;
#[test]
fn preview_drag_uses_remaining_right_hand_region() {
    assert_eq!(preview_percent_at(60, 20, 100), 60);
    assert_eq!(preview_percent_at(5, 20, 100), 75);
    assert_eq!(preview_percent_at(200, 20, 100), 25);
}
