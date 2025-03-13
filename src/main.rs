use std::{
    env, io::{BufRead, BufReader, Write}, net::TcpStream, str::FromStr, time::Duration
};

use anyhow::Error;
use chrono::Utc;
use clap::{Parser, command};
use env_logger::Builder;
use hnefatafl::{
    board::state::BitfieldBoardState,
    pieces::Side,
    play::Play,
    preset::{boards, rules},
};
use hnefatafl_copenhagen::{
    VERSION_ID,
    ai::{AI, AiBanal},
    color::Color,
    game::Game,
    play::{Plae, Vertex},
    role::Role,
    status::Status,
};
use hnefatafl_egui::ai::{Ai, BasicAi};
use log::{debug, LevelFilter};

// Move 26, defender wins, corner escape, time per move 15s 2025-03-06.

// Fixme: It takes way too long to evaluate the score when there are hardly any moves.

const PORT: &str = ":49152";

/// A Hnefatafl Copenhagen AI
///
/// This is an AI client that connects to a server.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(long)]
    username: String,

    #[arg(default_value = "", long)]
    password: String,

    /// attacker or defender
    #[arg(long)]
    role: Role,

    /// Connect to the HTP server at host
    #[arg(default_value = "hnefatafl.org", long)]
    host: String,

    /// Join game with id
    #[arg(long)]
    join_game: Option<u64>,

    /// Whether the application is being run by systemd
    #[arg(long)]
    systemd: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    init_logger(args.systemd);

    let mut username = "ai-".to_string();
    username.push_str(&args.username);

    let mut address = args.host.to_string();
    address.push_str(PORT);

    let mut buf = String::new();
    let mut tcp = TcpStream::connect(address)?;
    let mut reader = BufReader::new(tcp.try_clone()?);

    tcp.write_all(format!("{VERSION_ID} login {username} {}\n", args.password).as_bytes())?;
    reader.read_line(&mut buf)?;
    assert_eq!(buf, "= login\n");
    buf.clear();

    let color = Color::from(&args.role);
    loop {
        let game_id;

        if let Some(game_id_) = args.join_game {
            game_id = game_id_.to_string();
            tcp.write_all(format!("join_game_pending {game_id}\n").as_bytes())?;
        } else {
            new_game(&mut tcp, args.role, &mut reader, &mut buf)?;

            let message: Vec<_> = buf.split_ascii_whitespace().collect();
            game_id = message[3].to_string();
            buf.clear();

            wait_for_challenger(&mut reader, &mut buf, &mut tcp, &game_id)?;
        }

        let game = Game::default();
        let game_: hnefatafl::game::Game<BitfieldBoardState<u128>> =
            hnefatafl::game::Game::new(rules::COPENHAGEN, boards::COPENHAGEN).unwrap();

        debug!("\n{}", game.board);

        let ai_1 = hnefatafl_egui::ai::BasicAi::new(
            game_.logic,
            side_from_role(args.role),
            Duration::from_secs(15),
        );
        let ai_2 = Box::new(AiBanal);

        handle_messages(
            ai_1,
            ai_2,
            game,
            game_,
            &game_id,
            &color,
            &mut reader,
            &mut tcp,
        )?;

        if args.join_game.is_some() {
            return Ok(());
        }
    }
}

// "= new_game game GAME_ID ai-00 _ rated fischer 900000 10 _ false {}\n"
fn new_game(
    tcp: &mut TcpStream,
    role: Role,
    reader: &mut BufReader<TcpStream>,
    buf: &mut String,
) -> anyhow::Result<()> {
    tcp.write_all(format!("new_game {role} rated fischer 900000 10\n").as_bytes())?;

    loop {
        // "= new_game game GAME_ID ai-00 _ rated fischer 900000 10 _ false {}\n"
        reader.read_line(buf)?;

        if buf.trim().is_empty() {
            return Err(Error::msg("the TCP stream has closed"));
        }

        let message: Vec<_> = buf.split_ascii_whitespace().collect();
        if message[1] == "new_game" {
            return Ok(());
        }

        buf.clear();
    }
}

fn wait_for_challenger(
    reader: &mut BufReader<TcpStream>,
    buf: &mut String,
    tcp: &mut TcpStream,
    game_id: &str,
) -> anyhow::Result<()> {
    loop {
        reader.read_line(buf)?;

        if buf.trim().is_empty() {
            return Err(Error::msg("the TCP stream has closed"));
        }

        let message: Vec<_> = buf.split_ascii_whitespace().collect();
        if Some("challenge_requested") == message.get(1).copied() {
            debug!("{message:?}");
            buf.clear();

            break;
        }

        buf.clear();
    }

    tcp.write_all(format!("join_game {game_id}\n").as_bytes())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_messages(
    mut ai_1: BasicAi,
    mut ai_2: Box<dyn AI>,
    mut game: Game,
    mut game_: hnefatafl::game::Game<BitfieldBoardState<u128>>,
    game_id: &str,
    color: &Color,
    reader: &mut BufReader<TcpStream>,
    tcp: &mut TcpStream,
) -> anyhow::Result<()> {
    let mut buf = String::new();
    loop {
        reader.read_line(&mut buf)?;

        if buf.trim().is_empty() {
            return Err(Error::msg("the TCP stream has closed"));
        }

        let message: Vec<_> = buf.split_ascii_whitespace().collect();

        if Some("generate_move") == message.get(2).copied() {
            let Ok((mut play_game_, info)) = ai_1.next_play(&game_.state) else {
                panic!("we got an error from ai.next_play");
            };

            debug!("{info:?}\n");

            let mut play_game = Plae::from_str_(&play_game_.to_string(), color)?;

            debug!("{}", play_game.to_string().trim());

            if game.play(&play_game).is_err() {
                play_game = game
                    .generate_move(&mut ai_2)
                    .expect("the game must be in progress");

                let Plae::Play(play) = &play_game else {
                    panic!("the player can't resign");
                };

                play_game_ = Play::from_str(&format!(
                    "{}-{}",
                    play.from.fmt_other(),
                    play.to.fmt_other()
                ))
                .unwrap();

                debug!("changed play to: {}", play_game.to_string().trim());

                game.play(&play_game)?;
            };

            if let Err(invalid_play) = game_.do_play(play_game_) {
                debug!("invalid_play: {invalid_play:?}");
                tcp.write_all(format!("game {game_id} play {color} resigns _\n").as_bytes())?;
                return Ok(());
            }

            tcp.write_all(format!("game {game_id} {play_game}").as_bytes())?;
            debug!("{}", game.board);

            if game.status != Status::Ongoing {
                return Ok(());
            }
        } else if Some("play") == message.get(2).copied() {
            let Some(color) = message.get(3).copied() else {
                panic!("expected color");
            };
            let Ok(color) = Color::from_str(color) else {
                panic!("expected color to be a color");
            };

            let Some(from) = message.get(4).copied() else {
                panic!("expected from");
            };
            if from == "resigns" {
                return Ok(());
            }
            let Ok(from) = Vertex::from_str(from) else {
                panic!("expected from to be a vertex");
            };

            let Some(to) = message.get(5).copied() else {
                panic!("expected to");
            };
            let Ok(to) = Vertex::from_str(to) else {
                panic!("expected to to be a vertex");
            };

            let play = format!("play {color} {from} {to}\n");
            debug!("{}", play.trim());
            game.read_line(&play)?;

            if game.status != Status::Ongoing {
                return Ok(());
            }

            let play = format!("{}-{}", from.fmt_other(), to.fmt_other());
            let play = Play::from_str(&play).unwrap();

            if let Err(invalid_play) = game_.do_play(play) {
                debug!("invalid_play: {invalid_play:?}");
                tcp.write_all(format!("game {game_id} play {color} resigns _\n").as_bytes())?;
                return Ok(());
            }

            debug!("{}", game.board);
        } else if Some("game_over") == message.get(1).copied() {
            return Ok(());
        }

        buf.clear();
    }
}

#[must_use]
fn side_from_role(role: Role) -> Side {
    match role {
        Role::Attacker => Side::Attacker,
        Role::Defender => Side::Defender,
    }
}

fn init_logger(systemd: bool) {
    let mut builder = Builder::new();

    if systemd {
        builder.format(|formatter, record| {
            writeln!(formatter, "[{}]: {}", record.level(), record.args())
        });
    } else {
        builder.format(|formatter, record| {
            writeln!(
                formatter,
                "{} [{}] ({}): {}",
                Utc::now().format("%Y-%m-%d %H:%M:%S %z"),
                record.level(),
                record.target(),
                record.args()
            )
        });
    }

    if let Ok(var) = env::var("RUST_LOG") {
        builder.parse_filters(&var);
    } else {
        // if no RUST_LOG provided, default to logging at the Info level
        builder.filter(None, LevelFilter::Info);
    }

    builder.init();
}