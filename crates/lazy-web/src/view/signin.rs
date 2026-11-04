//! Signing in, and creating an account where the server takes them.
//!
//! The forms are drawn once and left alone while the person types; only the line under them
//! ([`status`]) changes as the request goes and comes back.

use lazy_client_core::{ClientCore, SessionPhase};
use lazy_protocol::ServerRegistration;

use super::html::{esc, in_shell, push};

/// The sign-in form, or the registration form.
pub(crate) fn form(core: &ClientCore, registering: bool) -> String {
    let registration = core.server_facts().map(|facts| facts.registration);
    let can_register = registration != Some(ServerRegistration::Closed);
    let needs_code = registration == Some(ServerRegistration::InviteOnly)
        || core
            .server_facts()
            .is_some_and(|facts| facts.setup_code_required);
    let mut out =
        String::from("<div class=\"signin\"><div class=\"card\"><div class=\"brand\">Lazy</div>");
    if registering && can_register {
        out.push_str("<h1>Create your account</h1><form id=\"register-form\" autocomplete=\"on\">");
        out.push_str("<label>Your name<input name=\"display_name\" autocomplete=\"name\" required maxlength=\"80\"></label>");
        out.push_str("<label>Username<input name=\"username\" autocomplete=\"username\" required autocapitalize=\"none\" spellcheck=\"false\"></label>");
        out.push_str("<label>Password<input name=\"password\" type=\"password\" autocomplete=\"new-password\" required minlength=\"8\"></label>");
        push!(
            out,
            "<label><span>Invitation code{}</span><input name=\"invite_code\" autocomplete=\"off\" autocapitalize=\"characters\" spellcheck=\"false\"{}></label>",
            if needs_code {
                ""
            } else {
                " <span class=\"muted\">(if you have one)</span>"
            },
            if needs_code { " required" } else { "" }
        );
        out.push_str("<div id=\"signin-status\" class=\"form-status\" role=\"status\"></div>");
        out.push_str("<button type=\"submit\" class=\"primary wide\" id=\"signin-submit\">Create account</button></form>");
        out.push_str("<p class=\"switch\">Have an account? <button type=\"button\" class=\"link\" data-action=\"show-sign-in\">Sign in</button></p>");
    } else {
        out.push_str("<h1>Sign in</h1><form id=\"signin-form\" autocomplete=\"on\">");
        // The hint stays outside the label, or it becomes part of the field's name for screen readers.
        out.push_str("<div class=\"field\"><label>Username<input name=\"username\" autocomplete=\"username\" required autocapitalize=\"none\" spellcheck=\"false\" autofocus aria-describedby=\"username-hint\"></label><span class=\"field-hint\" id=\"username-hint\">The name you chose when you joined. In the Lazy app, click your name at the bottom left: it is under Profile.</span></div>");
        out.push_str("<label>Password<input name=\"password\" type=\"password\" autocomplete=\"current-password\" required></label>");
        out.push_str("<div id=\"signin-status\" class=\"form-status\" role=\"status\"></div>");
        out.push_str("<button type=\"submit\" class=\"primary wide\" id=\"signin-submit\">Sign in</button></form>");
        if can_register {
            out.push_str("<p class=\"switch\">New here? <button type=\"button\" class=\"link\" data-action=\"show-register\">Create an account</button></p>");
        }
    }
    if in_shell() {
        out.push_str("<p class=\"switch\"><button type=\"button\" class=\"link\" data-action=\"change-server\">Change server</button></p>");
        out.push_str("<p class=\"fine\">For drop-ins, use Lazy on a computer.</p></div></div>");
    } else {
        out.push_str(
            "<p class=\"fine\">Lazy in your browser. For drop-ins, use the Lazy app.</p></div></div>",
        );
    }
    out
}

/// The line under the form: the last failure, or that the request is on its way.
pub(crate) fn status(core: &ClientCore) -> (String, bool) {
    match core.phase() {
        SessionPhase::SigningIn => ("Signing in…".to_owned(), true),
        SessionPhase::SignedOut {
            last_error: Some(error),
        } => (
            format!("<span class=\"error\">{}</span>", esc(error)),
            false,
        ),
        _ => (String::new(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazy_client_core::{Input, StartupState};

    #[test]
    fn only_a_phone_app_offers_to_change_server() {
        let mut core = ClientCore::new();
        core.handle(Input::Started(StartupState::default()));
        assert!(!form(&core, false).contains("change-server"));
        assert!(form(&core, false).contains("in your browser"));
        crate::view::html::set_in_shell(true);
        let html = form(&core, false);
        crate::view::html::set_in_shell(false);
        assert!(html.contains("data-action=\"change-server\""));
        assert!(!html.contains("in your browser"));
    }

    #[test]
    fn the_form_offers_registration_unless_the_server_is_closed() {
        let mut core = ClientCore::new();
        core.handle(Input::Started(StartupState::default()));
        let html = form(&core, false);
        assert!(html.contains("id=\"signin-form\""));
        assert!(html.contains("data-action=\"show-register\""));
        let html = form(&core, true);
        assert!(html.contains("id=\"register-form\""));
        assert!(html.contains("(if you have one)"));
        core.handle(Input::ServerFacts {
            url: core.server().url.clone(),
            facts: Some(lazy_client_core::ServerFacts {
                registration: ServerRegistration::Closed,
                setup_code_required: false,
            }),
        });
        let html = form(&core, true);
        assert!(
            html.contains("id=\"signin-form\""),
            "a closed server only signs in"
        );
        assert!(!html.contains("show-register"));
    }

    #[test]
    fn a_failed_sign_in_says_why() {
        let mut core = ClientCore::new();
        core.handle(Input::Started(StartupState::default()));
        core.handle(Input::SignIn {
            username: "ada".into(),
            password: "correct horse battery".into(),
        });
        assert_eq!(status(&core), ("Signing in…".to_owned(), true));
        core.handle(Input::AuthFailed(lazy_client_core::RequestError::Rejected(
            lazy_protocol::ApiError::new(
                lazy_protocol::ErrorCode::InvalidCredentials,
                "wrong username or password",
            ),
        )));
        let (html, busy) = status(&core);
        assert!(!busy);
        assert!(html.contains("class=\"error\""), "{html}");
    }
}
