//! Linux counterpart of Better Teams' SignInWebController.swift.
//! The helper process owns GTK's event loop; Iced/wgpu stays in the parent.
//! Native libraries are loaded at runtime so development headers are not needed.
//! See third_party/better-teams for the source and retained license.

use crate::i18n::tr;
use crate::auth::{WebViewReply, WebViewRequest, is_native_callback};
use libloading::Library;
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong, c_void};
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::process::{Command, Stdio};
use std::ptr;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

fn localized_label(message: &'static str) -> CString {
    CString::new(tr(message)).expect("label without NUL")
}

pub const HELPER_ARG: &str = "--auth-webview-helper";
const MAX_MESSAGE: u64 = 65536;

pub(crate) fn capture(request: WebViewRequest) -> Result<WebViewReply, String> {
    let exe = std::env::current_exe()
        .map_err(|_| tr("Could not find the sign-in window.").to_owned())?;
    let mut child = Command::new(exe)
        .arg(HELPER_ARG)
        .arg(format!("--language={}", crate::i18n::language().tag()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| tr("Could not open the sign-in window.").to_owned())?;
    let written = (|| {
        let mut input = child.stdin.take().ok_or(tr("The sign-in channel is unavailable."))?;
        serde_json::to_writer(&mut input, &request)
            .map_err(|_| tr("Could not start the sign-in window."))?;
        input
            .flush()
            .map_err(|_| tr("Could not start the sign-in window."))
    })();
    if let Err(message) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(message.into());
    }
    let deadline = Instant::now() + Duration::from_secs(610);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(75)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("A janela de login encerrou ou expirou. Entre novamente.".into());
            }
        }
    }
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .ok_or(tr("The sign-in window did not respond."))?
        .take(MAX_MESSAGE)
        .read_to_end(&mut output)
        .map_err(|_| tr("Could not receive a response from the sign-in window.").to_owned())?;
    serde_json::from_slice(&output)
        .map_err(|_| tr("The sign-in window could not start WebKitGTK.").to_owned())
}

pub fn helper_main() -> i32 {
    // Closing the parent also closes its authentication window.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    }
    let reply = (|| {
        if unsafe { libc::getppid() } <= 1 {
            return Ok(WebViewReply::Cancelled);
        }
        let mut input = String::new();
        std::io::stdin()
            .take(MAX_MESSAGE)
            .read_to_string(&mut input)
            .map_err(|_| tr("Could not receive the sign-in request.").to_owned())?;
        let request: WebViewRequest = serde_json::from_str(&input)
            .map_err(|_| tr("Invalid sign-in request.").to_owned())?;
        run(request)
    })()
    .unwrap_or_else(|message| WebViewReply::Failed { message });
    // Auth codes travel only over this private pipe, never a command line or log.
    if serde_json::to_writer(std::io::stdout().lock(), &reply).is_err() {
        1
    } else {
        0
    }
}

type Widget = *mut c_void;
type Callback = Option<unsafe extern "C" fn()>;

struct Api {
    // WebKit has worker threads. Keep libraries loaded until the helper exits.
    _libraries: Vec<ManuallyDrop<Library>>,
    init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) -> c_int,
    window_new: unsafe extern "C" fn(c_int) -> Widget,
    window_title: unsafe extern "C" fn(Widget, *const c_char),
    window_size: unsafe extern "C" fn(Widget, c_int, c_int),
    box_new: unsafe extern "C" fn(c_int, c_int) -> Widget,
    add: unsafe extern "C" fn(Widget, Widget),
    pack: unsafe extern "C" fn(Widget, Widget, c_int, c_int, c_uint),
    label_new: unsafe extern "C" fn(*const c_char) -> Widget,
    label_text: unsafe extern "C" fn(Widget, *const c_char),
    label_wrap: unsafe extern "C" fn(Widget, c_int),
    label_width: unsafe extern "C" fn(Widget, c_int),
    button_new: unsafe extern "C" fn(*const c_char) -> Widget,
    show: unsafe extern "C" fn(Widget),
    destroy: unsafe extern "C" fn(Widget),
    main: unsafe extern "C" fn(),
    quit: unsafe extern "C" fn(),
    connect: unsafe extern "C" fn(
        Widget,
        *const c_char,
        Callback,
        Widget,
        Option<unsafe extern "C" fn(Widget, Widget)>,
        c_uint,
    ) -> c_ulong,
    timer: unsafe extern "C" fn(
        c_uint,
        Option<unsafe extern "C" fn(Widget) -> c_int>,
        Widget,
    ) -> c_uint,
    remove_timer: unsafe extern "C" fn(c_uint) -> c_int,
    unref: unsafe extern "C" fn(Widget),
    free: unsafe extern "C" fn(Widget),
    error_free: unsafe extern "C" fn(Widget),
    context_new: unsafe extern "C" fn() -> Widget,
    web_new: unsafe extern "C" fn(Widget) -> Widget,
    load: unsafe extern "C" fn(Widget, *const c_char),
    stop: unsafe extern "C" fn(Widget),
    decision_action: unsafe extern "C" fn(Widget) -> Widget,
    action_request: unsafe extern "C" fn(Widget) -> Widget,
    request_uri: unsafe extern "C" fn(Widget) -> *const c_char,
    ignore: unsafe extern "C" fn(Widget),
    current_uri: unsafe extern "C" fn(Widget) -> *const c_char,
    evaluate: unsafe extern "C" fn(
        Widget,
        *const c_char,
        isize,
        *const c_char,
        *const c_char,
        Widget,
        Option<unsafe extern "C" fn(Widget, Widget, Widget)>,
        Widget,
    ),
    evaluate_finish: unsafe extern "C" fn(Widget, Widget, *mut Widget) -> Widget,
    value_string: unsafe extern "C" fn(Widget) -> *mut c_char,
}

impl Api {
    fn load() -> Result<Self, String> {
        // These are stable GTK3 / GLib / WebKitGTK 4.1 C ABI entry points.
        unsafe {
            let gtk = Library::new("libgtk-3.so.0")
                .map_err(|_| tr("GTK3 is unavailable for sign-in.").to_owned())?;
            let object = Library::new("libgobject-2.0.so.0")
                .map_err(|_| tr("GObject is unavailable.").to_owned())?;
            let glib = Library::new("libglib-2.0.so.0")
                .map_err(|_| tr("GLib is unavailable.").to_owned())?;
            let web = Library::new("libwebkit2gtk-4.1.so.0").map_err(|_| tr("WebKitGTK 4.1 is missing. Use a browser sign-in code or install webkit2gtk4.1.").to_owned())?;
            let javascript = Library::new("libjavascriptcoregtk-4.1.so.0")
                .map_err(|_| tr("JavaScriptCoreGTK is unavailable.").to_owned())?;
            macro_rules! symbol {
                ($library:ident, $name:literal) => {
                    *$library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|_| format!("Incompatible sign-in library: {}.", $name))?
                };
            }
            Ok(Self {
                init: symbol!(gtk, "gtk_init_check"),
                window_new: symbol!(gtk, "gtk_window_new"),
                window_title: symbol!(gtk, "gtk_window_set_title"),
                window_size: symbol!(gtk, "gtk_window_set_default_size"),
                box_new: symbol!(gtk, "gtk_box_new"),
                add: symbol!(gtk, "gtk_container_add"),
                pack: symbol!(gtk, "gtk_box_pack_start"),
                label_new: symbol!(gtk, "gtk_label_new"),
                label_text: symbol!(gtk, "gtk_label_set_text"),
                label_wrap: symbol!(gtk, "gtk_label_set_line_wrap"),
                label_width: symbol!(gtk, "gtk_label_set_max_width_chars"),
                button_new: symbol!(gtk, "gtk_button_new_with_label"),
                show: symbol!(gtk, "gtk_widget_show_all"),
                destroy: symbol!(gtk, "gtk_widget_destroy"),
                main: symbol!(gtk, "gtk_main"),
                quit: symbol!(gtk, "gtk_main_quit"),
                connect: symbol!(object, "g_signal_connect_data"),
                timer: symbol!(glib, "g_timeout_add_seconds"),
                remove_timer: symbol!(glib, "g_source_remove"),
                unref: symbol!(object, "g_object_unref"),
                free: symbol!(glib, "g_free"),
                error_free: symbol!(glib, "g_error_free"),
                context_new: symbol!(web, "webkit_web_context_new_ephemeral"),
                web_new: symbol!(web, "webkit_web_view_new_with_context"),
                load: symbol!(web, "webkit_web_view_load_uri"),
                stop: symbol!(web, "webkit_web_view_stop_loading"),
                decision_action: symbol!(
                    web,
                    "webkit_navigation_policy_decision_get_navigation_action"
                ),
                action_request: symbol!(web, "webkit_navigation_action_get_request"),
                request_uri: symbol!(web, "webkit_uri_request_get_uri"),
                ignore: symbol!(web, "webkit_policy_decision_ignore"),
                current_uri: symbol!(web, "webkit_web_view_get_uri"),
                evaluate: symbol!(web, "webkit_web_view_evaluate_javascript"),
                evaluate_finish: symbol!(web, "webkit_web_view_evaluate_javascript_finish"),
                value_string: symbol!(javascript, "jsc_value_to_string"),
                _libraries: vec![
                    ManuallyDrop::new(gtk),
                    ManuallyDrop::new(object),
                    ManuallyDrop::new(glib),
                    ManuallyDrop::new(web),
                    ManuallyDrop::new(javascript),
                ],
            })
        }
    }
}

struct Ui {
    api: Api,
    web: Widget,
    label: Widget,
    start: CString,
    redirect: String,
    outcome: RefCell<Option<WebViewReply>>,
    finished: Cell<bool>,
    needs_browser: Cell<bool>,
    deadline: Instant,
    parent: u32,
}

impl Ui {
    fn finish(&self, outcome: WebViewReply) {
        if self.finished.replace(true) {
            return;
        }
        *self.outcome.borrow_mut() = Some(outcome);
        unsafe {
            (self.api.stop)(self.web);
            (self.api.quit)();
        }
    }

    fn status(&self, message: &str) {
        unsafe {
            (self.api.label_text)(self.label, CString::new(message).expect("label without NUL").as_ptr());
        }
    }

    fn require_browser(&self) {
        self.needs_browser.set(true);
        self.status(tr("This step requires a browser that supports passkeys or your organization's policy. Select Use a code in the browser."));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignInStep {
    Account,
    Password,
    Authenticator,
    Passkey,
    Continue,
}

impl SignInStep {
    fn hint(self) -> &'static str {
        match self {
            // Conta corporativa: conta, depois senha, e só então o Authenticator.
            Self::Account => {
                tr("Sign in with your account, then your password and Authenticator.")
            }
            Self::Password => tr("Enter your password. Authenticator will ask you to confirm after that."),
            Self::Authenticator => tr("Confirm access in Authenticator."),
            Self::Passkey => {
                tr("This window does not support passkeys. Select Cancel on that page and follow the account, password, and Authenticator prompts.")
            }
            Self::Continue => {
                tr("Follow the account, password, and Authenticator prompts. Confirm in the app after entering your password.")
            }
        }
    }
}

fn contains_any(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| text.contains(phrase))
}

/// Work-account sign-in is account, then password, then Authenticator. The passkey
/// error page is the WebKit dead end and must not be described as the
/// Authenticator step.
fn sign_in_step(url: &str, page_text: &str) -> SignInStep {
    if needs_browser(url, page_text) {
        return SignInStep::Passkey;
    }
    let text = page_text.to_lowercase();
    if contains_any(
        &text,
        &[
            "enter password",
            "enter your password",
            "use your password",
            "insira a senha",
            "digite a senha",
            "inserir senha",
            "usar senha",
            "usar sua senha",
        ],
    ) {
        return SignInStep::Password;
    }
    if contains_any(
        &text,
        &[
            "authenticator",
            "approve a request",
            "sign-in request",
            "sign in request",
            "aprovar uma solicitação",
            "enter the code",
            "insira o código",
            "digite o código",
        ],
    ) {
        return SignInStep::Authenticator;
    }
    if contains_any(
        &text,
        &[
            "email, phone, or skype",
            "e-mail, telefone ou skype",
            "sign in",
            "entrar",
        ],
    ) {
        return SignInStep::Account;
    }
    SignInStep::Continue
}

// Port of Better Teams' SignInPageCheck, including the passkey error page
// displayed by Entra when this WebKit platform cannot perform WebAuthn.
fn needs_browser(url: &str, page_text: &str) -> bool {
    if let Ok(url) = url::Url::parse(url) {
        let host = url.host_str().unwrap_or("");
        if host.starts_with("certauth.") {
            return true;
        }
        let microsoft_login = ["login.microsoft.com", "login.microsoftonline.com"]
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")));
        if microsoft_login && url.path().to_ascii_lowercase().contains("/fido/") {
            return true;
        }
    }
    let text = page_text.to_lowercase();
    if [
        "aadsts50097",
        "aadsts53000",
        "aadsts53001",
        "aadsts53002",
        "aadsts53003",
    ]
    .iter()
    .any(|code| text.contains(code))
    {
        return true;
    }
    (text.contains("passkey") || text.contains("chave de acesso"))
        && [
            "does not support",
            "doesn't support",
            "not supported",
            "não suporta",
            "não é compatível",
        ]
        .iter()
        .any(|phrase| text.contains(phrase))
}

// All callbacks run on GTK's helper main thread. `Ui` outlives its widgets and
// timer, and each signal is connected using the C signature of that signal.
unsafe extern "C" fn decide_policy(
    _web: Widget,
    decision: Widget,
    kind: c_int,
    data: Widget,
) -> c_int {
    let ui = unsafe { &*(data as *const Ui) };
    if ui.finished.get() || !matches!(kind, 0 | 1) {
        return 0;
    }
    let action = unsafe { (ui.api.decision_action)(decision) };
    if action.is_null() {
        return 0;
    }
    let request = unsafe { (ui.api.action_request)(action) };
    if request.is_null() {
        return 0;
    }
    let uri = unsafe { (ui.api.request_uri)(request) };
    if uri.is_null() {
        return 0;
    }
    let Ok(uri) = (unsafe { CStr::from_ptr(uri) }).to_str() else {
        return 0;
    };
    if is_native_callback(uri, &ui.redirect) {
        let callback = uri.to_owned();
        unsafe {
            (ui.api.ignore)(decision);
        }
        ui.finish(WebViewReply::Callback { url: callback });
        return 1;
    }
    if needs_browser(uri, "") {
        ui.require_browser();
    }
    if kind == 1 {
        // Handle target=_blank on the same authentication surface.
        if let Ok(uri) = CString::new(uri) {
            unsafe {
                (ui.api.ignore)(decision);
                (ui.api.load)(ui.web, uri.as_ptr());
            }
            return 1;
        }
    }
    0
}

unsafe extern "C" fn close_window(_window: Widget, _event: Widget, data: Widget) -> c_int {
    unsafe { &*(data as *const Ui) }.finish(WebViewReply::Cancelled);
    1
}

unsafe extern "C" fn cancel(_button: Widget, data: Widget) {
    unsafe { &*(data as *const Ui) }.finish(WebViewReply::Cancelled);
}

unsafe extern "C" fn use_code(_button: Widget, data: Widget) {
    unsafe { &*(data as *const Ui) }.finish(WebViewReply::UseDeviceCode);
}

unsafe extern "C" fn retry(_button: Widget, data: Widget) {
    let ui = unsafe { &*(data as *const Ui) };
    ui.needs_browser.set(false);
    ui.status(tr("Opening Microsoft sign-in…"));
    unsafe {
        (ui.api.load)(ui.web, ui.start.as_ptr());
    }
}

unsafe extern "C" fn load_changed(_web: Widget, event: c_int, data: Widget) {
    let ui = unsafe { &*(data as *const Ui) };
    if ui.finished.get() || event != 3 {
        return;
    }
    // Visible text only, including shadow roots and same-origin frames.
    // The login form often lives there, so body.innerText misses the
    // passkey error. Never read input values or log the text.
    unsafe {
        Rc::increment_strong_count(data as *const Ui);
        (ui.api.evaluate)(
            ui.web,
            c"(function(){function t(n){if(!n)return '';var s='';try{if(n.shadowRoot)s+=t(n.shadowRoot);}catch(e){}var c=n.childNodes||[];for(var i=0;i<c.length;i++){var x=c[i];if(x.nodeType===3)s+=x.textContent||'';else s+=t(x);}return s;}function d(doc){try{return doc&&doc.body?t(doc.body):'';}catch(e){return '';}}var p=[d(document)];var f=document.querySelectorAll('iframe');for(var i=0;i<f.length&&i<6;i++){try{p.push(d(f[i].contentDocument));}catch(e){}}return p.join('\\n').slice(0,20000);})()".as_ptr(),
            -1,
            ptr::null(),
            ptr::null(),
            ptr::null_mut(),
            Some(page_checked),
            data,
        );
    }
}

unsafe extern "C" fn page_checked(source: Widget, result: Widget, data: Widget) {
    let ui = unsafe { Rc::from_raw(data as *const Ui) };
    let mut error = ptr::null_mut();
    let value = unsafe { (ui.api.evaluate_finish)(source, result, &mut error) };
    if !error.is_null() {
        unsafe {
            (ui.api.error_free)(error);
        }
    }
    if value.is_null() {
        return;
    }
    let text = unsafe { (ui.api.value_string)(value) };
    if !text.is_null() {
        if !ui.finished.get() {
            let uri = unsafe { (ui.api.current_uri)(source) };
            let uri = if uri.is_null() {
                ""
            } else {
                unsafe { CStr::from_ptr(uri) }.to_str().unwrap_or("")
            };
            let page_text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
            if !page_text.trim().is_empty() {
                let step = sign_in_step(uri, &page_text);
                ui.needs_browser.set(step == SignInStep::Passkey);
                ui.status(step.hint());
            }
        }
        unsafe {
            (ui.api.free)(text.cast());
        }
    }
    unsafe {
        (ui.api.unref)(value);
    }
}

unsafe extern "C" fn load_failed(
    _web: Widget,
    _event: c_int,
    _uri: *const c_char,
    _error: Widget,
    data: Widget,
) -> c_int {
    let ui = unsafe { &*(data as *const Ui) };
    if !ui.finished.get() {
        ui.status(tr("The page failed to load. Try again or use a browser sign-in code."));
    }
    0
}

unsafe extern "C" fn tick(data: Widget) -> c_int {
    let ui = unsafe { &*(data as *const Ui) };
    if Instant::now() >= ui.deadline || unsafe { libc::getppid() } as u32 != ui.parent {
        ui.finish(WebViewReply::Cancelled);
    }
    1
}

fn run(request: WebViewRequest) -> Result<WebViewReply, String> {
    let start = CString::new(request.authorize_url)
        .map_err(|_| tr("Invalid sign-in URL.").to_owned())?;
    let api = Api::load()?;
    unsafe {
        if (api.init)(ptr::null_mut(), ptr::null_mut()) == 0 {
            return Err(tr("Could not connect the sign-in window to the desktop session.").into());
        }
        let window = (api.window_new)(0);
        let root = (api.box_new)(1, 10);
        let footer = (api.box_new)(0, 10);
        let label = (api.label_new)(localized_label("Opening Microsoft sign-in…").as_ptr());
        (api.label_wrap)(label, 1);
        (api.label_width)(label, 68);
        let context = (api.context_new)();
        let web = (api.web_new)(context);
        (api.unref)(context);
        if window.is_null() || root.is_null() || web.is_null() {
            return Err(tr("Could not create the sign-in window.").into());
        }
        (api.window_title)(window, localized_label("Sign in with Microsoft — Teams Linux").as_ptr());
        (api.window_size)(window, 600, 740);
        (api.add)(window, root);
        (api.pack)(root, web, 1, 1, 0);
        (api.pack)(root, label, 0, 0, 4);
        (api.pack)(root, footer, 0, 0, 10);
        let retry_button = (api.button_new)(localized_label("Try again").as_ptr());
        let code_button = (api.button_new)(localized_label("Use a code in the browser").as_ptr());
        let cancel_button = (api.button_new)(localized_label("Cancel").as_ptr());
        for button in [retry_button, code_button, cancel_button] {
            (api.pack)(footer, button, 1, 1, 8);
        }
        let ui = Rc::new(Ui {
            api,
            web,
            label,
            start,
            redirect: request.redirect_uri,
            outcome: RefCell::new(None),
            finished: Cell::new(false),
            needs_browser: Cell::new(false),
            deadline: Instant::now() + Duration::from_secs(600),
            parent: libc::getppid() as u32,
        });
        let signal_owner = Rc::into_raw(Rc::clone(&ui));
        let data = signal_owner.cast_mut().cast::<c_void>();
        macro_rules! signal {
            ($widget:expr, $name:literal, $callback:expr, $signature:ty) => {
                (ui.api.connect)(
                    $widget,
                    concat!($name, "\0").as_ptr().cast(),
                    Some(std::mem::transmute::<$signature, unsafe extern "C" fn()>(
                        $callback,
                    )),
                    data,
                    None,
                    0,
                )
            };
        }
        signal!(
            web,
            "decide-policy",
            decide_policy,
            unsafe extern "C" fn(Widget, Widget, c_int, Widget) -> c_int
        );
        signal!(
            web,
            "load-changed",
            load_changed,
            unsafe extern "C" fn(Widget, c_int, Widget)
        );
        signal!(
            web,
            "load-failed",
            load_failed,
            unsafe extern "C" fn(Widget, c_int, *const c_char, Widget, Widget) -> c_int
        );
        signal!(
            window,
            "delete-event",
            close_window,
            unsafe extern "C" fn(Widget, Widget, Widget) -> c_int
        );
        signal!(
            retry_button,
            "clicked",
            retry,
            unsafe extern "C" fn(Widget, Widget)
        );
        signal!(
            code_button,
            "clicked",
            use_code,
            unsafe extern "C" fn(Widget, Widget)
        );
        signal!(
            cancel_button,
            "clicked",
            cancel,
            unsafe extern "C" fn(Widget, Widget)
        );
        let timer = (ui.api.timer)(1, Some(tick), data);
        (ui.api.show)(window);
        (ui.api.load)(web, ui.start.as_ptr());
        (ui.api.main)();
        (ui.api.remove_timer)(timer);
        (ui.api.destroy)(window);
        drop(Rc::from_raw(signal_owner));
        let outcome = ui
            .outcome
            .borrow_mut()
            .take()
            .unwrap_or(WebViewReply::Cancelled);
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::{SignInStep, needs_browser, sign_in_step};

    #[test]
    fn work_account_sign_in_is_account_then_password_then_authenticator() {
        let passkey = "Passkey not registered. This browser or device does not support passkeys. Try a different browser or device.";
        assert_eq!(
            sign_in_step("https://login.microsoftonline.com/signin", passkey),
            SignInStep::Passkey
        );
        assert_eq!(
            sign_in_step(
                "https://login.microsoftonline.com/signin",
                "Enter your email, phone, or Skype."
            ),
            SignInStep::Account
        );
        assert_eq!(
            sign_in_step("https://login.microsoftonline.com/signin", "Enter password"),
            SignInStep::Password
        );
        assert_eq!(
            sign_in_step("https://login.microsoftonline.com/signin", "Insira a senha"),
            SignInStep::Password
        );
        assert_eq!(
            sign_in_step(
                "https://login.microsoftonline.com/signin",
                "Choose a passkey or use your password"
            ),
            SignInStep::Password
        );
        assert_eq!(
            sign_in_step(
                "https://login.microsoftonline.com/signin",
                "Approve a request on your Microsoft Authenticator app"
            ),
            SignInStep::Authenticator
        );
    }

    #[test]
    fn detects_passkey_limit_and_upstream_conditional_access_cases() {
        assert!(needs_browser(
            "https://login.microsoftonline.com/signin",
            "Passkey not registered. This browser or device does not support passkeys."
        ));
        assert!(needs_browser(
            "https://login.microsoftonline.com/fido/get",
            ""
        ));
        assert!(needs_browser(
            "https://certauth.login.microsoftonline.com/auth",
            ""
        ));
        assert!(needs_browser(
            "https://login.microsoftonline.com/error",
            "AADSTS53000: Device must be compliant."
        ));
        assert!(!needs_browser(
            "https://example.test/fido/login",
            "Enter your password"
        ));
        assert!(!needs_browser(
            "https://login.microsoftonline.com/signin",
            "Choose a passkey or use your password"
        ));
    }
}
