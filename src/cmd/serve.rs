use super::command_prelude::*;
#[cfg(feature = "watch")]
use super::watch;
use crate::{get_book_dir, get_build_opts, open};
use clap::builder::NonEmptyStringValueParser;
use futures_util::sink::SinkExt;
use futures_util::StreamExt;
use http::Uri;
use mdbook::errors::*;
use mdbook::utils;
use mdbook::utils::fs::get_404_output_file;
use mdbook::MDBook;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use tokio::sync::broadcast;
use warp::ws::Message;
use warp::Filter;

/// The HTTP endpoint for the websocket used to trigger reloads when a file changes.
const LIVE_RELOAD_ENDPOINT: &str = "__livereload";

// Create clap subcommand arguments
pub fn make_subcommand() -> Command {
    Command::new("serve")
        .about("Serves a book at http://localhost:3000, and rebuilds it on changes")
        .arg_dest_dir()
        .arg_root_dir()
        .arg(
            Arg::new("hostname")
                .short('n')
                .long("hostname")
                .num_args(1)
                .default_value("localhost")
                .value_parser(NonEmptyStringValueParser::new())
                .help("Hostname to listen on for HTTP connections"),
        )
        .arg(
            Arg::new("port")
                .short('p')
                .long("port")
                .num_args(1)
                .default_value("3000")
                .value_parser(NonEmptyStringValueParser::new())
                .help("Port to use for HTTP connections"),
        )
        .arg_open()
        .arg_language()
}

// Serve command implementation
pub fn execute(args: &ArgMatches) -> Result<()> {
    let book_dir = get_book_dir(args);
    let build_opts = get_build_opts(args);
    let mut book = MDBook::load_with_build_opts(book_dir, build_opts.clone())?;

    let port = args.get_one::<String>("port").unwrap();
    let hostname = args.get_one::<String>("hostname").unwrap();
    let open_browser = args.get_flag("open");

    let address = format!("{}:{}", hostname, port);

    let update_config = |book: &mut MDBook| {
        book.config
            .set("output.html.live-reload-endpoint", LIVE_RELOAD_ENDPOINT)
            .expect("live-reload-endpoint update failed");
        if let Some(dest_dir) = args.get_one::<PathBuf>("dest-dir") {
            book.config.build.build_dir = dest_dir.into();
        }
        // Override site-url for local serving of the 404 file
        book.config.set("output.html.site-url", "/").unwrap();
    };
    update_config(&mut book);
    book.build()?;

    let language: Option<String> = match build_opts.language_ident {
        // index.html will be at the root directory.
        Some(_) => None,
        None => match book.config.default_language() {
            // If book has translations, index.html will be under src/en/ or
            // similar.
            Some(lang_ident) => Some(lang_ident.clone()),
            // If not, it will be at the root.
            None => None,
        },
    };

    let sockaddr: SocketAddr = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("no address found for {}", address))?;
    let build_dir = book.build_dir_for("html");
    let input_404 = book
        .config
        .get("output.html.input-404")
        .and_then(toml::Value::as_str)
        .map(ToString::to_string);
    let file_404 = get_404_output_file(&input_404);

    // A channel used to broadcast to any websockets to reload when a file changes.
    let (tx, _rx) = tokio::sync::broadcast::channel::<Message>(100);

    let reload_tx = tx.clone();
    let thread_handle = std::thread::spawn(move || {
        serve(build_dir, sockaddr, reload_tx, &file_404, language);
    });

    let serving_url = format!("http://{}", address);
    info!("Serving on: {}", serving_url);

    if open_browser {
        open(serving_url);
    }

    #[cfg(feature = "watch")]
    watch::trigger_on_change(&book, move |paths, book_dir| {
        info!("Files changed: {:?}", paths);
        info!("Building book...");

        // FIXME: This area is really ugly because we need to re-set livereload :(
        let result =
            MDBook::load_with_build_opts(&book_dir, build_opts.clone()).and_then(|mut b| {
                update_config(&mut b);
                b.build()
            });

        if let Err(e) = result {
            error!("Unable to load the book");
            utils::log_backtrace(&e);
        } else {
            let _ = tx.send(Message::text("reload"));
        }
    });

    let _ = thread_handle.join();

    Ok(())
}

#[tokio::main]
async fn serve(
    build_dir: PathBuf,
    address: SocketAddr,
    reload_tx: broadcast::Sender<Message>,
    file_404: &str,
    language: Option<String>,
) {
    // A warp Filter which captures `reload_tx` and provides an `rx` copy to
    // receive reload messages.
    let sender = warp::any().map(move || reload_tx.subscribe());

    // A warp Filter to handle the livereload endpoint. This upgrades to a
    // websocket, and then waits for any filesystem change notifications, and
    // relays them over the websocket.
    let livereload = warp::path(LIVE_RELOAD_ENDPOINT)
        .and(warp::ws())
        .and(sender)
        .map(|ws: warp::ws::Ws, mut rx: broadcast::Receiver<Message>| {
            ws.on_upgrade(move |ws| async move {
                let (mut user_ws_tx, _user_ws_rx) = ws.split();
                trace!("websocket got connection");
                if let Ok(m) = rx.recv().await {
                    trace!("notify of reload");
                    let _ = user_ws_tx.send(m).await;
                }
            })
        });
    // A warp Filter that serves from the filesystem.
    let book_route = warp::fs::dir(build_dir.clone());

    std::panic::set_hook(Box::new(move |panic_info| {
        // exit if serve panics
        error!("Unable to serve: {}", panic_info);
        std::process::exit(1);
    }));

    if let Some(lang_ident) = language {
        // Redirect root to the default translation directory, if serving a localized book.
        // NOTE: This can't be `/{lang_ident}`, or the static assets won't get loaded.
        // BUG: Redirects get cached if you change the --language parameter,
        // meaning you'll get a 404 unless you disable the cache in Developer
        // Tools.
        let index_for_language = format!("/{}/index.html", lang_ident)
            .parse::<Uri>()
            .unwrap();
        let redirect_to_index =
            warp::path::end().map(move || warp::redirect(index_for_language.clone()));

        // BUG: It is not possible to conditionally redirect to the correct 404
        // page depending on the URL using warp, so just redirect to the one in
        // the default language.
        // See: https://github.com/seanmonstar/warp/issues/171
        let fallback_route = warp::fs::file(build_dir.join(lang_ident).join(file_404))
            .map(|reply| warp::reply::with_status(reply, warp::http::StatusCode::NOT_FOUND));

        let routes = livereload
            .or(redirect_to_index)
            .or(book_route)
            .or(fallback_route);
        warp::serve(routes).run(address).await;
    } else {
        // The fallback route for 404 errors
        let fallback_route = warp::fs::file(build_dir.join(file_404))
            .map(|reply| warp::reply::with_status(reply, warp::http::StatusCode::NOT_FOUND));

        let routes = livereload.or(book_route).or(fallback_route);
        warp::serve(routes).run(address).await;
    };
}
