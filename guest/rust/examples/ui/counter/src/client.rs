use ambient_api::prelude::*;

#[element_component]
fn App(hooks: &mut Hooks) -> Element {
    let (count, set_count) = hooks.use_state(0);
    FlowColumn::el([
        Text::el(format!("We've counted to {count} now")),
        Button::new("Increase", move |_| set_count(count + 1)).el(),
    ])
    .with_padding_even(STREET)
    .with(space_between_items(), STREET)
}

#[main]
pub fn main() {
    App.el().spawn_interactive();
}
