//! OAuth2 authentication providers.

use std::io::Read;
use std::collections::HashSet;
use std::sync::{Mutex, Arc};
use std::error::Error;

use iron::prelude::*;
use iron::status;
use iron::Url as IronUrl;
use iron::Handler;
use iron::modifiers::Redirect;
use iron::typemap::Key;
use router::Router;
use persistent::Write;
use rand::{OsRng, Rng};
use hyper::Client;
use hyper::Url as HyperUrl;
use hyper::header::{Accept, qitem};
use hyper::mime::{Mime, TopLevel, SubLevel};
use rustc_serialize::json;
use postgres::Connection;

use error::{FictResult, fict_err, as_fict_err};
use model::{Database, User, Session};

mod connection;
mod github;

pub use self::github::GitHub;

/// Initial size of the "valid state parameter" pool.
const INIT_STATE_CAPACITY: usize = 100;

/// Length of the "state" parameter used to defeat XSS hijacking.
const STATE_LEN: usize = 20;

/// Configuration options that are common to all supported OAuth providers.
#[derive(Clone)]
struct Options {
    name: &'static str,
    root: &'static str,
    client_id: String,
    client_secret: String,
    request_uri: IronUrl,
    token_uri: HyperUrl,
}

/// Mutable state to be shared among the request handlers installed by a specific `Provider`.
pub struct Shared {
    rng: OsRng,
    valid_states: HashSet<String>,
}

impl Shared {

    /// Initialize the shared state to a reasonable starting point.
    fn new() -> Shared {
        Shared{
            rng: OsRng::new().unwrap(),
            valid_states: HashSet::with_capacity(INIT_STATE_CAPACITY),
        }
    }

    /// Generate an unguessable random string for use as a `state` parameter. Remember it as valid.
    fn generate_state(&mut self) -> String {
        let state: String = self.rng.gen_ascii_chars().take(STATE_LEN).collect();
        self.valid_states.insert(state.clone());
        state
    }

    /// Verify that a given state is valid. Discard it from the provider's store if it is.
    fn validate_state(&mut self, state: &str) -> bool {
        self.valid_states.remove(state)
    }

}

struct RequestHandler<P: Provider> {
    provider: P
}

impl <P: Provider> Handler for RequestHandler<P> {

    fn handle(&self, r: &mut Request) -> IronResult<Response> {
        self.provider.request_handler(r)
    }

}

struct CallbackHandler<P: Provider> {
    provider: P
}

impl <P: Provider> Handler for CallbackHandler<P> {

    fn handle(&self, r: &mut Request) -> IronResult<Response> {
        self.provider.callback_handler(r)
    }

}

/// Extract an access token from an OAuth provider's JSON response.
#[derive(RustcDecodable)]
struct TokenResponse {
    access_token: String,
}

/// Common behavior and general flow shared among OAuth providers.
pub trait Provider : Key + Send + Sync + Clone {

    /// Access the common Provider options.
    fn options(&self) -> &Options;

    /// Access the `Mutex` containing the persistent state for this provider from a specific
    /// request. Panics if the persistence middleware has not been installed.
    fn shared_mutex(&self, req: &mut Request) -> Arc<Mutex<Shared>>;

    /// Specify the scopes to request from this provider during the authorization process, in the
    /// format expected by the provider.
    fn scopes(&self) -> &'static str;

    /// Use the authorization token acquired on behalf of the authenticating user to acquire the
    /// user's username and email address from the provider's API.
    fn get_user_data(&self, token: &str) -> FictResult<(String, String)>;

    /// Create the middleware that will appropriately register `Shared` state for this provider.
    fn link(&self, chain: &mut Chain);

    /// Generate the route for the `request_handler`.
    fn request_glob(&self) -> String {
        let o = self.options();
        format!("{}/{}", o.root, o.name)
    }

    /// Generate the route for the `callback_handler`.
    fn callback_glob(&self) -> String {
        let o = self.options();
        format!("{}/{}/callback", o.root, o.name)
    }

    /// Generate the full URL to the `callback_handler`.
    fn callback_url(&self) -> IronUrl {
        IronUrl::parse(&format!("http://localhost:3000/{}", &self.callback_glob())).unwrap()
    }

    /// *Phase 1:* Redirect to the OAuth provider's authorization page with a randomly generated
    /// `state` parameter.
    fn request_handler(&self, req: &mut Request) -> IronResult<Response> {
        let o = self.options();

        let mutex = self.shared_mutex(req);
        let mut shared = mutex.lock().unwrap();
        let state = shared.generate_state();

        let mut u = o.request_uri.clone();
        u.query = Some(format!(
            "client_id={}&redirect_uri={}&scope={}&state={}",
            &o.client_id, self.callback_url(), self.scopes(), &state
        ));

        debug!("Redirecting to provider {}: [{}].", o.name, u);

        Ok(Response::with((status::Found, Redirect(u))))
    }

    /// *Phase 2:* Accept the redirect back from the OAuth provider. Validate the `state` and
    /// exchange the `code` for an access token. Use the access token with the provider's API
    /// to locate the authenticated user's username and email address.
    fn callback_handler(&self, req: &mut Request) -> IronResult<Response> {
        let mutex = req.get::<Write<Database>>().unwrap_or_else(|_| {
            panic!("No database connection available");
        });
        let pool = mutex.lock().unwrap();
        let conn = pool.get().unwrap();

        let mutex = self.shared_mutex(req);
        let mut shared = mutex.lock().unwrap();

        let result = self.extract_callback_params(req)
            .and_then(|(code, state)| self.validate_state(&mut *shared, &state).map(|_| { code }))
            .and_then(|code| self.generate_token(code))
            .and_then(|token| self.find_user(&*conn, token))
            .and_then(|user| Session::assign(&*conn, user, &mut shared.rng));

        match result {
            Ok(session) => {
                debug!("OAuth flow completed. Acquired {}.", session);

                let output = format!("You've successfully created a session: [{}]", session.token);
                Ok(Response::with((status::Ok, output)))
            },
            Err(message) => {
                warn!("OAuth flow problem: {}", message);

                Ok(Response::with((status::BadRequest, message.description())))
            },
        }
    }

    /// Extract the "code" and "state" query parameters from the callback request. Fail if either are
    /// not present.
    fn extract_callback_params(&self, req: &Request) -> FictResult<(String, String)> {
        let u = req.url.clone().into_generic_url();

        let mut code_op: Option<String> = None;
        let mut state_op: Option<String> = None;

        match u.query_pairs() {
            Some(pairs) => {
                for pair in pairs.iter() {
                    let (ref key, ref value) = *pair;
                    let key_str: &str = key;

                    match key_str {
                        "code" => code_op = Some(value.clone()),
                        "state" => state_op = Some(value.clone()),
                        _ => warn!("Unrecognized callback parameter: [{}]", &key),
                    }
                }
            },
            None => {
                warn!("Callback request missing any query parameters: [{}]", u);
                return Err(fict_err("Callback missing query parameters"));
            },
        };

        match (code_op, state_op) {
            (Some(code), Some(state)) => Ok((code, state)),
            _ => Err(fict_err("Callback request missing required query parameters")),
        }
    }

    /// Ensure that the `state` returned by the OAuth provider is one that was generated by this
    /// service.
    fn validate_state(&self, shared: &mut Shared, state: &str) -> FictResult<()> {
        if shared.validate_state(state) {
            Ok(())
        } else {
            Err(fict_err("Unfamiliar state encountered. Danger: this could be an XSS attack!"))
        }
    }

    /// Exchange a `code` obtained through an OAuth handshake for an access token.
    fn generate_token(&self, code: String) -> FictResult<String> {
        let o = self.options();

        let b: &str = &format!("client_id={}&client_secret={}&code={}",
            &o.client_id, &o.client_secret, &code
        );

        debug!("Attempting to acquire a {} access token from: [{}]", o.name, o.token_uri);

        let client = Client::new();
        let mut req = client.post(o.token_uri.clone()).body(b);
        req = req.header(Accept(vec![qitem(Mime(TopLevel::Application, SubLevel::Json, vec![]))]));

        req.send()
            .map_err(as_fict_err)
            .and_then(|mut response| {
                let mut body = String::new();
                match response.read_to_string(&mut body) {
                    Ok(_) => Ok(body),
                    Err(_) => Err(fict_err("Unable to read response")),
                }
            })
            .and_then(|body| json::decode(&body).map_err(as_fict_err))
            .map(|token_resp: TokenResponse| token_resp.access_token)
    }

    fn find_user(&self, conn: &Connection, token: String) -> FictResult<User> {
        let (email, username) = try!(self.get_user_data(&token));
        User::find_or_create(conn, email, username)
    }

    /// Register the routes necessary to support this Provider. Usually, this will involve a
    /// *redirect route*, which will redirect to an external authorization page, and a *callback
    /// route*, to which the provider is expected to return control with a redirect back.
    fn route(&self, router: &mut Router) {
        router.get(self.request_glob(), RequestHandler{provider: self.clone()});
        router.get(self.callback_glob(), CallbackHandler{provider: self.clone()});
    }

}
