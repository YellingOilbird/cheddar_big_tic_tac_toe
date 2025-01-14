use near_sdk::{
    AccountId, Balance, BorshStorageKey, Gas, Duration, PanicOnDefault,
    Promise, PromiseOrValue, PromiseResult, assert_one_yocto
};
use near_sdk::{
    env, ext_contract, log, near_bindgen, ONE_NEAR, ONE_YOCTO, require
};
use near_sdk::json_types::U128;
use near_sdk::borsh::{self, BorshSerialize, BorshDeserialize};
use near_sdk::serde::{Serialize, Deserialize};
use near_sdk::collections::{UnorderedMap, UnorderedSet};
use stats::UserPenalties;
use views::GameLimitedView;

mod board;
mod callbacks;
mod config;
mod game;
mod game_config;
mod internal;
mod player;
mod stats;
mod token_receiver;
mod views;
mod utils;

use crate::board::*;
use crate::config::*;
use crate::game::*;
use crate::game_config::*;
use crate::player::*;
use crate::stats::*;
use crate::token_receiver::*;
use crate::utils::*;
use crate::views::GameResult;

#[derive(BorshSerialize, BorshStorageKey)]
pub enum StorageKey {
    WhitelistedTokens,
    Games,
    StoredGames,
    Players,
    /* * */
    Stats,
    Affiliates {account_id : AccountId},
    TotalRewards {account_id : AccountId},
    TotalAffiliateRewards {account_id : AccountId}
}

pub (crate) type MinDeposit = Balance;

#[near_bindgen]
#[derive(BorshSerialize, BorshDeserialize, PanicOnDefault)]
pub struct Contract {
    /// Allowed game reward tokens as `TokenContractId` : `MinDeposit`
    whitelisted_tokens: UnorderedMap<TokenContractId, MinDeposit>,
    games: UnorderedMap<GameId, Game>,
    available_players: UnorderedMap<AccountId, GameConfig>,
    /* * */
    stats: UnorderedMap<AccountId, Stats>,
    /// `GameId` which will be set for next created `Game`
    next_game_id: GameId,
    /// service fee percentage in BASIS_P (see `config.rs`)
    service_fee_percentage: u32,
    /// max expected game duration in nanoseconds (see `config.rs`)
    max_game_duration: Duration,
    /// referrer fee percentage from service_fee_percentage in BASIS_P (see `config.rs`)
    referrer_ratio: u32,
    /// system updates
    pub last_update_timestamp: u64,
    /// max expected turn duration in nanoseconds (max_game_duration / max possible turns num)
    max_turn_duration: u64,
    /// storage for printing results
    pub max_stored_games: u8,
    pub stored_games: UnorderedMap<GameId, GameLimitedView>
}

#[near_bindgen]
impl Contract {
    #[init]
    pub fn new(config: Option<Config>) -> Self {
        let (
            service_fee_percentage, 
            max_game_duration,
            referrer_ratio,
            max_stored_games
        ) = if let Some(config) = config {
            config.assert_valid();
            (
                config.service_fee_percentage,
                sec_to_nano(config.max_game_duration_sec),
                config.referrer_ratio,
                config.max_stored_games
            )
        } else {
            // default config
            (
                // 10% total fees - 9.5% to referrer, 0.5% to cheddar distribution
                MAX_FEES,
                // 1 hour for max_game_duration will be set
                // also 400 sec will be max turn duration (max_game_duration / MAX_TURNS_NUM)
                sec_to_nano(60 * 60),
                // 95% refferer fees from 10% total fees
                9500,
                // 50 last games will be stored
                50
            )
        };
        Self {
            whitelisted_tokens: UnorderedMap::new(StorageKey::WhitelistedTokens),
            games: UnorderedMap::new(StorageKey::Games),
            available_players: UnorderedMap::new(StorageKey::Players),
            stats: UnorderedMap::new(StorageKey::Stats),
            next_game_id: 0,
            service_fee_percentage,
            max_game_duration,
            referrer_ratio,
            last_update_timestamp: 0,
            max_turn_duration: max_game_duration / MAX_NUM_TURNS,
            max_stored_games,
            stored_games: UnorderedMap::new(StorageKey::StoredGames)
        }
    }

    /// Make player available only with NEAR deposits
    #[payable]
    pub fn make_available(
        &mut self,
        game_config: Option<GameConfigNear>,
    ) {
        let cur_timestamp = env::block_timestamp();
        // checkpoint
        self.internal_ping_expired_players(cur_timestamp);

        let account_id: &AccountId = &env::predecessor_account_id();
        assert!(self.available_players.get(account_id).is_none(), "Already in the waiting list the list");

        let deposit: Balance = env::attached_deposit();
        assert!(deposit >= MIN_DEPOSIT_NEAR, "Deposit is too small. Attached: {}, Required: {}", deposit, MIN_DEPOSIT_NEAR);

        let (opponent_id, referrer_id) = if let Some(game_config) = game_config {
            (game_config.opponent_id, game_config.referrer_id.clone())
        } else {
            (None, None)
        };

        self.available_players.insert(account_id,
            &GameConfig {
                token_id: AccountId::new_unchecked("near".into()),
                deposit,
                opponent_id,
                referrer_id: referrer_id.clone(),
                created_at: cur_timestamp
            }
        );
        
        self.internal_check_player_available(&account_id);

        if let Some(referrer_id) = referrer_id {
            self.internal_add_referrer( &account_id, &referrer_id);
        }
    }

    #[payable]
    pub fn make_unavailable(&mut self) {
        assert_one_yocto();
        let account_id = env::predecessor_account_id();
        match self.available_players.get(&account_id) {
            Some(config) => {
                // refund players deposit
                let token_id = config.token_id.clone();
                self.available_players.remove(&account_id);

                self.internal_transfer(&token_id, &account_id, config.deposit.into())
                    .then(Self::ext(env::current_account_id())
                    .with_static_gas(CALLBACK_GAS)
                    .transfer_deposit_callback(account_id, &config)
                );
            },
            None => panic!("You are not available now")
        }
    }

    pub fn start_game(&mut self, player_2_id: AccountId) -> GameId {
        if let Some(player_2_config) = self.available_players.get(&player_2_id) {
            // Check is game initiator (predecessor) player available to play as well
            let player_1_id = env::predecessor_account_id();
            assert_ne!(player_1_id.clone(), player_2_id.clone(), "Find a friend to play");

            // Get predecessor's available deposit
            let player_1_config = self.internal_get_available_player(&player_1_id);
            let player_1_config_token = player_1_config.token_id;
            let player_1_deposit = player_1_config.deposit;

            self.internal_check_player_available(&player_1_id);
            
            if let Some(player_id) = player_2_config.opponent_id {
                assert_eq!(player_id, player_1_id, "Wrong account");
            }

            // Deposits from two players must be equal
            assert_eq!(
                player_1_deposit, 
                player_2_config.deposit, 
                "Mismatched deposits for players! You: {}, Opponent {}",
                player_1_deposit,
                player_2_config.deposit
            );

            let game_id = self.next_game_id;
            let token_id = player_2_config.token_id;

            assert_eq!(token_id, player_1_config_token, "Mismatch tokens! Choosen tokens for opponent and you must be the same");
            // deposit * 2
            let balance = match player_2_config.deposit.checked_mul(2) {
                Some(value) => value,
                None => panic!("multiplication overflow, too big deposit amount"),
            };

            let reward = GameDeposit {
                token_id: token_id.clone(),
                balance: balance.into()
            };
            log!("game reward:{} in token {:?} ", balance, token_id.clone());
            
            let seed = near_sdk::env::random_seed();
            let mut game = match seed[0] % 2 {
                0 => {
                    Game::create_game(
                    player_2_id.clone(),
                    player_1_id.clone(),
                    reward
                    )
                },
                _ => {
                    Game::create_game(
                    player_1_id.clone(),
                    player_2_id.clone(),
                    reward
                    )
                },
            };

            game.change_state(GameState::Active);
            self.games.insert(&game_id, &game);

            self.next_game_id += 1;
            self.available_players.remove(&player_1_id);
            self.available_players.remove(&player_2_id);

            if let Some(referrer_id) = player_1_config.referrer_id {
                self.internal_add_referrer(&player_1_id, &referrer_id);
            }
            if let Some(referrer_id) = player_2_config.referrer_id {
                self.internal_add_referrer(&player_2_id, &referrer_id);
            }

            self.internal_update_stats(Some(&token_id), &player_1_id, UpdateStatsAction::AddPlayedGame, None, None);
            self.internal_update_stats(Some(&token_id), &player_2_id, UpdateStatsAction::AddPlayedGame, None, None);
            game_id
        } else {
            panic!("Your opponent is not ready");
        }
    }

    pub fn make_move(&mut self, game_id: &GameId, row: usize, col: usize) -> [[Option<Piece>; BOARD_SIZE]; BOARD_SIZE] {
        let cur_timestamp = env::block_timestamp();
        //checkpoint
        self.internal_ping_expired_games(cur_timestamp);

        let mut game = self.internal_get_game(game_id);
        let init_game_state = game.game_state;

        assert_eq!(env::predecessor_account_id(), game.current_player_account_id(), "No access");
        assert_eq!(init_game_state, GameState::Active, "Current game isn't active");

        match game.board.check_move(row, col) {
            Ok(_) => {
                // fill board tile with current player piece
                game.board.tiles[row][col] = Some(game.current_piece);
                // switch piece to other one
                game.current_piece = game.current_piece.other();
                // switch player
                game.current_player_index = 1 - game.current_player_index;
                game.board.update_winner(row, col);

                if let Some(winner) = game.board.winner {
                    // change game state to Finished
                    game.change_state(GameState::Finished);
                    self.internal_update_game(game_id, &game);
                    // get winner account, if there is Tie - refund to both players
                    // with crop service fee amount from it
                    let winner_account:Option<&AccountId> = match winner {
                        board::Winner::X => game.get_player_acc_by_piece(Piece::X),
                        board::Winner::O => game.get_player_acc_by_piece(Piece::O),
                        board::Winner::Tie => None,
                    };
               
                    let balance = if winner_account.is_some() {
                        // SOME WINNER
                        log!("\nGame over! {} won!", winner_account.unwrap());
                        self.internal_distribute_reward(game_id, winner_account)
                    } else {
                        // TIE
                        log!("\nGame over! Tie!");
                        self.internal_distribute_reward(game_id, None)
                    };

                    let game_result = match winner_account {
                        Some(winner) => GameResult::Win(winner.clone()),
                        None => GameResult::Tie,
                    };

                    let (player1, player2) = game.get_player_accounts();

                    let game_to_store = GameLimitedView{
                        game_result,
                        player1,
                        player2,
                        reward_or_tie_refund: GameDeposit {
                            token_id: game.reward().token_id,
                            balance
                        },
                        board: game.board.tiles,
                    };

                    self.internal_store_game(game_id, game_to_store);
                    self.internal_stop_game(game_id);
                    
                    return game.board.tiles;
                };
            },
            Err(e) => match e {
                MoveError::GameAlreadyOver => panic!("Game is already finished"),
                MoveError::InvalidPosition { row, col } => panic!(
                    "Provided position is invalid: row: {} col: {}", row, col),
                MoveError::TileFilled { other_piece, row, col } => panic!(
                    "The tile row: {} col: {} already contained another piece: {:?}", row, col, other_piece
                ),
            },
        }
        if game.game_state == GameState::Active {

            game.total_turns += 1;
            // previous turn timestamp
            let previous_turn_timestamp = game.last_turn_timestamp;
            // this turn timestamp
            game.last_turn_timestamp = cur_timestamp;
            // this game duration 
            game.current_duration = cur_timestamp - game.initiated_at;

            if previous_turn_timestamp == 0 {
                if cur_timestamp - game.initiated_at > self.max_turn_duration {
                    log!("Turn duration expired. Required:{} Current:{} ", self.max_turn_duration, cur_timestamp - game.initiated_at);
                    // looser - current player
                    self.internal_stop_expired_game(game_id, env::predecessor_account_id());
                    return game.board.tiles;
                } else {
                    self.internal_update_game(game_id, &game);
                    return game.board.tiles;
                }
            }

            // expired turn time scenario - too long movement from current player
            if game.last_turn_timestamp - previous_turn_timestamp > self.max_turn_duration {
                log!("Turn duration expired. Required:{} Current:{} ", self.max_turn_duration, game.last_turn_timestamp - previous_turn_timestamp);
                // looser - current player
                self.internal_stop_expired_game(game_id, env::predecessor_account_id());
                return game.board.tiles;
            };

            if game.current_duration <= self.max_game_duration {
                self.internal_update_game(game_id, &game);
                return game.board.tiles;
            } else {
                log!("Game duration expired. Required:{} Current:{} ", self.max_game_duration, game.current_duration);
                // looser - current player
                self.internal_stop_expired_game(game_id, env::predecessor_account_id());
                return game.board.tiles;
            }
        } else {
            panic!("Something wrong with game id: {} state", game_id)
        }

    }

    #[payable]
    pub fn give_up(&mut self, game_id: &GameId) {
        assert_one_yocto();
        let mut game: Game = self.internal_get_game(&game_id);
        assert_eq!(game.game_state, GameState::Active, "Current game isn't active");
        
        let account_id = env::predecessor_account_id();

        let (player1, player2) = self.internal_get_game_players(game_id);
        
        let winner = if account_id == player1{
            player2.clone()
        } else if account_id == player2 {
            player1.clone()
        } else {
            panic!("You are not in this game. GameId: {} ", game_id)
        };

        let balance = self.internal_distribute_reward(game_id, Some(&winner));
        game.change_state(GameState::Finished);
        self.internal_update_game(game_id, &game);

        let game_to_store = GameLimitedView{
            game_result: GameResult::Win(winner),
            player1,
            player2,
            reward_or_tie_refund: GameDeposit {
                token_id: game.reward().token_id,
                balance
            },
            board: game.board.tiles,
        };

        self.internal_store_game(game_id, game_to_store);
        self.internal_stop_game(game_id);
    }

    pub fn stop_game(&mut self, game_id: &GameId) {
        let mut game: Game = self.internal_get_game(&game_id);
        assert_eq!(game.game_state, GameState::Active, "Current game isn't active");

        let account_id = env::predecessor_account_id();
        assert_ne!(env::predecessor_account_id(), game.current_player_account_id(), "No access");

        let (player1, player2) = self.internal_get_game_players(game_id);

        game.current_duration = env::block_timestamp() - game.initiated_at;
        log!("game.current_duration : {}", game.current_duration);
        log!("env::block_timestamp() : {}", env::block_timestamp());
        log!("game.initiated_at : {}", game.initiated_at);
        log!("self.max_game_duration : {}", self.max_game_duration);
        log!("game.last_turn_timestamp : {}", game.last_turn_timestamp);
        log!("self.max_turn_duration :{} ", self.max_turn_duration);
        assert!(
            game.current_duration >= self.max_game_duration || env::block_timestamp() - game.last_turn_timestamp > self.max_turn_duration, 
            "Too early to stop the game"
        );

        let (winner, looser) = if account_id == player1 {
            (player1, player2)
        } else if account_id == player2 {
            (player2, player1)
        } else {
            panic!("You are not in this game. GameId: {} ", game_id)
        };

        self.internal_update_stats(
            Some(&game.reward().token_id), 
            &looser, 
            UpdateStatsAction::AddPenaltyGame, 
            None, 
            None);

        let balance = self.internal_distribute_reward(game_id, Some(&winner));
        game.change_state(GameState::Finished);
        self.internal_update_game(game_id, &game);

        let game_to_store = GameLimitedView{
            game_result: GameResult::Win(winner.clone()),
            player1: winner,
            player2: looser,
            reward_or_tie_refund: GameDeposit {
                token_id: game.reward().token_id,
                balance
            },
            board: game.board.tiles,
        };

        self.internal_store_game(game_id, game_to_store);
        self.internal_stop_game(game_id);
    }
}

#[cfg(test)]
mod tests {
    use near_contract_standards::fungible_token::receiver::FungibleTokenReceiver;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::{testing_env, Balance};
    use crate::views::GameView;

    use super::*;

    const ONE_CHEDDAR:Balance = ONE_NEAR;

    fn user() -> AccountId {
        "user".parse().unwrap()
    }
    fn opponent() -> AccountId {
        "opponent.near".parse().unwrap()
    }
    fn referrer() -> AccountId {
        "referrer.near".parse().unwrap()
    }
    fn acc_cheddar() -> AccountId {
        "cheddar".parse().unwrap()
    }
    fn near() -> AccountId {
        "near".parse().unwrap()
    }

    fn setup_contract(
        predecessor: AccountId,
        service_fee_percentage: Option<u32>,
        referrer_fee: Option<u32>,
        max_game_duration_sec: Option<u32>
    ) -> (VMContextBuilder, Contract) {
        let mut context = VMContextBuilder::new();
        testing_env!(context.build());
        let config = if service_fee_percentage.is_none() && max_game_duration_sec.is_none() && referrer_fee.is_none(){
            None
        } else {
            Some(Config {
                service_fee_percentage: service_fee_percentage.unwrap(),
                referrer_ratio: referrer_fee.unwrap_or(BASIS_P / 2),
                max_game_duration_sec: max_game_duration_sec.unwrap(),
                max_stored_games: 50u8
            })
        };

        let contract = Contract::new(
            config
        );
        testing_env!(context
            .predecessor_account_id(predecessor.clone())
            .signer_account_id(predecessor.clone())
            .build());
        (context, contract)
    }

    fn whitelist_token(
        ctr: &mut Contract,
    ) {
        ctr.whitelist_token(acc_cheddar().clone(), U128(ONE_CHEDDAR / 10))
    }

    fn make_available_near(
        ctx: &mut VMContextBuilder,
        ctr: &mut Contract,
        user: &AccountId,
        amount: Balance,
        opponent_id: Option<AccountId>, 
        referrer_id: Option<AccountId> 
    ) {
        testing_env!(ctx
            .attached_deposit(amount)
            .predecessor_account_id(user.clone())
            .signer_account_id(user.clone())
            .build());
        ctr.make_available(Some(GameConfigNear { 
            opponent_id, 
            referrer_id 
        }));
    }

    fn make_available_ft(
        ctx: &mut VMContextBuilder,
        ctr: &mut Contract,
        user: &AccountId,
        amount: Balance,
        msg: String
    ) {
        testing_env!(ctx
            .attached_deposit(ONE_YOCTO)
            .predecessor_account_id(acc_cheddar().clone())
            .signer_account_id(user.clone())
            .build());
        ctr.ft_on_transfer(user.clone(), U128(amount), msg);
    }

    fn make_unavailable(
        ctx: &mut VMContextBuilder,
        ctr: &mut Contract,
        user: &AccountId,
    ) {
        testing_env!(ctx
            .attached_deposit(ONE_YOCTO)
            .predecessor_account_id(user.clone())
            .signer_account_id(user.clone())
            .build());
        ctr.make_unavailable();
    }

    fn start_game(
        ctx: &mut VMContextBuilder,
        ctr: &mut Contract,
        user: &AccountId,
        opponent: &AccountId,
    ) -> GameId {
        testing_env!(ctx
            .predecessor_account_id(user.clone())
            .build());
        ctr.start_game(opponent.clone())
    }

    fn make_move(
        ctx: &mut VMContextBuilder,
        ctr: &mut Contract,
        user: &AccountId,
        game_id: &GameId,
        row: usize,
        col: usize
    ) -> [[Option<Piece>; BOARD_SIZE]; BOARD_SIZE] {
        testing_env!(ctx
            .predecessor_account_id(user.clone())
            .build());
        ctr.make_move(game_id, row, col)
    }

    fn stop_game(
        ctx: &mut VMContextBuilder,
        ctr: &mut Contract,
        user: &AccountId,
        game_id: &GameId,
        forward_time_sec: u32
    ) {
        let nanos = sec_to_nano(forward_time_sec);
        testing_env!(ctx
            .predecessor_account_id(user.clone())
            .attached_deposit(ONE_YOCTO)
            .block_timestamp(nanos)
            .build());
        ctr.stop_game(&game_id)
    }

    fn get_board_current_player(game: &Game) -> AccountId {
        game.current_player_account_id()
    }

    /// This function is used to print out the board in a human readable way
    fn print_tiles(tiles: &[[Option<Piece>; BOARD_SIZE]; BOARD_SIZE]) {
        // The result of this function will be something like the following:
        //   A B C
        // 1 x ▢ ▢
        // 2 ▢ ▢ o
        // 3 ▢ ▢ ▢
        print!("  ");
        for j in 0..tiles[0].len() as u8 {
            // `b'A'` produces the ASCII character code for the letter A (i.e. 65)
            print!(" {}", (b'A' + j) as char);
        }
        // This prints the final newline after the row of column letters
        println!();
        for (i, row) in tiles.iter().enumerate() {
            // We print the row number with a space in front of it
            print!(" {}", i + 1);
            for tile in row {
                print!(" {}", match *tile {
                    Some(Piece::X) => "x",
                    Some(Piece::O) => "o",
                    None => "\u{25A2}", // empty tile pretty print "▢"
                });
            }
            println!();
        }
        println!();
    }

    fn game_basics() -> Result<(VMContextBuilder, Contract), std::io::Error> {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10)); // HERE
        assert!(ctr.get_available_players().is_empty());
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), (ONE_CHEDDAR / 10).into())
        ]));

        let gc1 = GameConfigArgs { 
            opponent_id: Some(opponent()), 
            referrer_id: Some(referrer()) 
        };
        let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
        let gc2 = GameConfigArgs { 
            opponent_id: Some(user()), 
            referrer_id: None 
        };
        let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
        make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
        make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        assert_eq!(ctr.get_available_players(), Vec::<(AccountId, GameConfigView)>::from([
            (user(), GameConfigView { 
                token_id: acc_cheddar(), 
                deposit: U128(ONE_CHEDDAR), 
                opponent_id: Some(opponent()), 
                referrer_id: Some(referrer()),
                created_at: 0
            }),
            (opponent(), GameConfigView { 
                token_id: acc_cheddar(), 
                deposit: U128(ONE_CHEDDAR), 
                opponent_id: Some(user()), 
                referrer_id: None,
                created_at: 0
            }),
        ]));

        let user2 = "user2".parse().unwrap();
        let opponent2 = "opponent2".parse().unwrap();

        make_available_near(&mut ctx, &mut ctr, &user2, ONE_NEAR, None, None);
        make_available_near(&mut ctx, &mut ctr, &opponent2, ONE_NEAR, None, None);

        let game_id_cheddar = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        let game_id_near = start_game(&mut ctx, &mut ctr, &user2, &opponent2);
        
        let game_cheddar = ctr.internal_get_game(&game_id_cheddar);
        let game_near = ctr.internal_get_game(&game_id_near);
        // cheddar
        let player_1_c = game_cheddar.current_player_account_id().clone();
        let player_2_c = game_cheddar.next_player_account_id().clone();
        // near
        let player_1_n = game_near.current_player_account_id().clone();
        let player_2_n = game_near.next_player_account_id().clone();

        assert!(ctr.get_active_games().contains(&(game_id_cheddar, GameView::from(&game_cheddar))));
        assert!(ctr.get_active_games().contains(&(game_id_near, GameView::from(&game_near))));

        // near game
        // 600000000000
        // 600000000000

        let mut tiles = make_move(&mut ctx, &mut ctr, &player_1_n, &game_id_near, 0, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2_n, &game_id_near, 0, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1_n, &game_id_near, 1, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2_n, &game_id_near, 2, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1_n, &game_id_near, 0, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2_n, &game_id_near, 2, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1_n, &game_id_near, 2, 1);
        print_tiles(&tiles);

        let player_1_stats = ctr.get_stats(&opponent2);
        let player_2_stats = ctr.get_stats(&user2);
        println!("{:#?}", player_1_stats);
        println!("{:#?}", player_2_stats);
        //todo
        // assert!(
        //     player_1_stats.games_played == player_2_stats.games_played
        // );
        // assert!(
        //     player_2_stats.victories_num == 0 && player_1_stats.victories_num == 1
        // );   
        // assert_eq!(
        //     player_1_stats.total_reward.clone(), Vec::from([
        //         (
        //             "near".parse().unwrap(), 
        //             (2 * ONE_NEAR - ((2 * ONE_NEAR / BASIS_P as u128 )* MIN_FEES as u128))
        //         )
        //     ])
        // );
        // assert!(player_2_stats.total_reward.is_empty());

        testing_env!(ctx
            .predecessor_account_id(player_2_c)
            .block_timestamp(ctr.max_game_duration + 1)
            .attached_deposit(ONE_YOCTO)
            .build()
        );
        ctr.stop_game(&game_id_cheddar);
        Ok((ctx, ctr))
    }

    #[test]
    fn test_whitelist_token() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), U128(ONE_CHEDDAR / 10))
        ]));
        assert!(ctr.get_available_players().is_empty());
    }
    #[test]
    fn make_available_unavailable_near() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        assert!(ctr.get_available_players().is_empty());
        make_available_near(&mut ctx, &mut ctr, &user(), ONE_NEAR, None, Some(referrer()));
        make_available_near(&mut ctx, &mut ctr, &opponent(), ONE_NEAR, Some(user()), None);
        assert_eq!(ctr.get_available_players(), Vec::<(AccountId, GameConfigView)>::from([
            (user(), GameConfigView { 
                token_id: near(), 
                deposit: U128(ONE_NEAR), 
                opponent_id: None, 
                referrer_id: Some(referrer()),
                created_at: 0
            }),
            (opponent(), GameConfigView { 
                token_id: near(), 
                deposit: U128(ONE_NEAR), 
                opponent_id: Some(user()), 
                referrer_id: None,
                created_at: 0
            }),
        ]));
        make_unavailable(&mut ctx, &mut ctr, &user());
        make_unavailable(&mut ctx, &mut ctr, &opponent());
        assert!(ctr.get_available_players().is_empty());
    }
    #[test]
    fn test_make_available_unavailable() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), (ONE_CHEDDAR / 10).into())
        ]));
        assert!(ctr.get_available_players().is_empty());
        let gc1 = GameConfigArgs { 
            opponent_id: Some(opponent()), 
            referrer_id: Some(referrer()) 
        };
        let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
        let gc2 = GameConfigArgs { 
            opponent_id: Some(user()), 
            referrer_id: None 
        };
        let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
        make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
        make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        assert_eq!(ctr.get_available_players(), Vec::<(AccountId, GameConfigView)>::from([
            (user(), GameConfigView { 
                token_id: acc_cheddar(), 
                deposit: U128(ONE_CHEDDAR), 
                opponent_id: Some(opponent()), 
                referrer_id: Some(referrer()),
                created_at: 0
            }),
            (opponent(), GameConfigView { 
                token_id: acc_cheddar(), 
                deposit: U128(ONE_CHEDDAR), 
                opponent_id: Some(user()), 
                referrer_id: None,
                created_at: 0
            }),
        ]));
        make_unavailable(&mut ctx, &mut ctr, &user());
        make_unavailable(&mut ctx, &mut ctr, &opponent());
        assert!(ctr.get_available_players().is_empty());
    }
    #[test]
    #[should_panic(expected="Mismatch tokens! Choosen tokens for opponent and you must be the same")]
    fn start_game_diff_tokens() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), (ONE_CHEDDAR / 10).into())
        ]));
        assert!(ctr.get_available_players().is_empty());
        let gc1 = GameConfigArgs { 
            opponent_id: Some(opponent()), 
            referrer_id: Some(referrer()) 
        };
        let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");

        make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
        make_available_near(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, None, None);
        start_game(&mut ctx, &mut ctr, &user(), &opponent());
    }
    #[test]
    fn test_give_up() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), (ONE_CHEDDAR / 10).into())
        ]));
        assert!(ctr.get_available_players().is_empty());
        let gc1 = GameConfigArgs { 
            opponent_id: Some(opponent()), 
            referrer_id: Some(referrer()) 
        };
        let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
        let gc2 = GameConfigArgs { 
            opponent_id: Some(user()), 
            referrer_id: None 
        };
        let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
        make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
        make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        assert_eq!(ctr.get_available_players(), Vec::<(AccountId, GameConfigView)>::from([
            (user(), GameConfigView { 
                token_id: acc_cheddar(), 
                deposit: U128(ONE_CHEDDAR), 
                opponent_id: Some(opponent()), 
                referrer_id: Some(referrer()),
                created_at: 0 
            }),
            (opponent(), GameConfigView { 
                token_id: acc_cheddar(), 
                deposit: U128(ONE_CHEDDAR), 
                opponent_id: Some(user()), 
                referrer_id: None,
                created_at: 0 
            }),
        ]));
        testing_env!(ctx
            .attached_deposit(ONE_YOCTO)
            .predecessor_account_id(user().clone())
            .build());
        let game_id = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        ctr.give_up(&game_id);
        let player_1_stats = ctr.get_stats(&user());
        let player_2_stats = ctr.get_stats(&opponent());

        assert!(
            player_1_stats.games_played == player_2_stats.games_played
        );
        assert!(
            player_1_stats.victories_num == 0 && player_2_stats.victories_num == 1
        );
        assert_eq!(
            player_2_stats.total_reward, Vec::from([(acc_cheddar(), (2 * ONE_CHEDDAR - ((2 * ONE_CHEDDAR / BASIS_P as u128) * MIN_FEES as u128)))]) 
        );
        assert!(player_1_stats.total_reward.is_empty());
    }
    #[test]
    fn test_game_basics() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), (ONE_CHEDDAR / 10).into())
        ]));
        assert!(ctr.get_available_players().is_empty());
        let gc1 = GameConfigArgs { 
            opponent_id: Some(opponent()), 
            referrer_id: Some(referrer()) 
        };
        let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
        let gc2 = GameConfigArgs { 
            opponent_id: Some(user()), 
            referrer_id: None 
        };
        let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
        make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
        make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        
        let game_id = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        
        let game = ctr.internal_get_game(&game_id);
        let player_1 = game.current_player_account_id().clone();
        let player_2 = game.next_player_account_id().clone();

        assert_ne!(player_1, game.next_player_account_id());
        assert_ne!(game.players[0].piece, game.players[1].piece);
        assert_eq!(player_1, game.players[0].account_id);
        assert_eq!(player_2, game.players[1].account_id);
        assert_eq!(game.board.current_piece, game.players[0].piece);

        assert!(ctr.get_active_games().contains(&(game_id, GameView::from(&game))));

        let mut tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 0, 4);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 1, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 1, 3);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 2, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 3, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 3, 3);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 4, 0);
        print_tiles(&tiles);
        // tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 4, 1);
        // print_tiles(&tiles);

        let player_1_stats = ctr.get_stats(&opponent());
        let player_2_stats = ctr.get_stats(&user());
        println!("{:#?}", player_1_stats);
        println!("{:#?}", player_2_stats);
        assert!(
            player_1_stats.games_played == player_2_stats.games_played
        );
        assert!(
            player_2_stats.victories_num == 1 && player_1_stats.victories_num == 0
        );   
        assert_eq!(
            player_2_stats.total_reward.clone(), Vec::from([
                (acc_cheddar(), (2 * ONE_CHEDDAR - ((2 * ONE_CHEDDAR / BASIS_P as u128 )* MIN_FEES as u128)))
            ])
        );
        assert!(player_1_stats.total_reward.is_empty());
    }

    #[test]
    fn test_game_basics_near() {
        assert!(game_basics().is_ok());
    }

    #[test]
    fn test_tie_scenario() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), (ONE_CHEDDAR / 10).into())
        ]));
        assert!(ctr.get_available_players().is_empty());
        let gc1 = GameConfigArgs { 
            opponent_id: Some(opponent()), 
            referrer_id: None 
        };
        let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
        let gc2 = GameConfigArgs { 
            opponent_id: Some(user()), 
            referrer_id: None 
        };
        let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
        make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
        make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        
        let game_id = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        
        let game = ctr.internal_get_game(&game_id);
        let player_1 = game.current_player_account_id().clone();
        let player_2 = game.next_player_account_id().clone();

        println!("( {} , {} )", player_1, player_2);

        assert_ne!(player_1, game.next_player_account_id());
        assert_ne!(game.players[0].piece, game.players[1].piece);
        assert_eq!(player_1, game.players[0].account_id);
        assert_eq!(player_2, game.players[1].account_id);
        assert_eq!(game.board.current_piece, game.players[0].piece);

        assert!(ctr.get_active_games().contains(&(game_id, GameView::from(&game))));

        let mut tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 0, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 0, 3);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 4);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 1, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 1, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 1, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 1, 3);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 1, 4);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 2, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 2, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 3);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 2, 4);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 3, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 3, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 3, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 3, 3);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 3, 4);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 4, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 4, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 4, 3);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 4, 4);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 4, 2);
        print_tiles(&tiles);

        let player_1_stats = ctr.get_stats(&opponent());
        let player_2_stats = ctr.get_stats(&user());

        assert!(
            player_1_stats.games_played == player_2_stats.games_played
        );
        assert_eq!(
            player_1_stats.victories_num, player_2_stats.victories_num
        );
        assert!(
            player_1_stats.total_reward == player_2_stats.total_reward
        ); 
        // assert_eq!(
        //     ctr.reward_computed,
        //     (2 * ONE_CHEDDAR - (2 * ONE_CHEDDAR / BASIS_P as u128 * MIN_FEES as u128)) / 2 
        // )
    }
    #[test]
    #[should_panic(expected="Too early to stop the game")]
    fn test_stop_game_too_early() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), U128(ONE_CHEDDAR / 10))
        ]));
        assert!(ctr.get_available_players().is_empty());
        let gc1 = GameConfigArgs { 
            opponent_id: Some(opponent()), 
            referrer_id: None 
        };
        let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
        let gc2 = GameConfigArgs { 
            opponent_id: Some(user()), 
            referrer_id: None 
        };
        let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
        make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
        make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        
        let game_id = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        
        let game = ctr.internal_get_game(&game_id);
        let player_1 = game.current_player_account_id().clone();
        let player_2 = game.next_player_account_id().clone();

        println!("( {} , {} )", player_1, player_2);

        assert_ne!(player_1, game.next_player_account_id());
        assert_ne!(game.players[0].piece, game.players[1].piece);
        assert_eq!(player_1, game.players[0].account_id);
        assert_eq!(player_2, game.players[1].account_id);
        assert_eq!(game.board.current_piece, game.players[0].piece);

        assert!(ctr.get_active_games().contains(&(game_id, GameView::from(&game))));

        let mut tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 0, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 2, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 1, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 1, 2);
        print_tiles(&tiles);
        
        stop_game(&mut ctx, &mut ctr, &player_2, &game_id, 20);
    }

    #[test]
    #[should_panic(expected="No access")]
    fn test_stop_game_wrong_access() {
        let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
        whitelist_token(&mut ctr);
        assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
            (acc_cheddar(), (ONE_CHEDDAR / 10).into())
        ]));
        assert!(ctr.get_available_players().is_empty());
        let gc1 = GameConfigArgs { 
            opponent_id: Some(opponent()), 
            referrer_id: None 
        };
        let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
        let gc2 = GameConfigArgs { 
            opponent_id: Some(user()), 
            referrer_id: None 
        };
        let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
        make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
        make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        
        let game_id = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        
        let game = ctr.internal_get_game(&game_id);
        let player_1 = game.current_player_account_id().clone();
        let player_2 = game.next_player_account_id().clone();

        println!("( {} , {} )", player_1, player_2);

        assert_ne!(player_1, game.next_player_account_id());
        assert_ne!(game.players[0].piece, game.players[1].piece);
        assert_eq!(player_1, game.players[0].account_id);
        assert_eq!(player_2, game.players[1].account_id);
        assert_eq!(game.board.current_piece, game.players[0].piece);

        assert!(ctr.get_active_games().contains(&(game_id, GameView::from(&game))));

        let mut tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 0, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 2, 1);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 2);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 1, 0);
        print_tiles(&tiles);
        tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 1, 2);
        print_tiles(&tiles);
        stop_game(&mut ctx, &mut ctr, &player_1, &game_id, 601);
    }

    // #[test]
    // fn test_expired_game() {
    //     let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  Some(60 * 10));
    //     whitelist_token(&mut ctr);
    //     assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
    //         (acc_cheddar(), (ONE_CHEDDAR / 10).into())
    //     ]));
    //     assert!(ctr.get_available_players().is_empty());
    //     let gc1 = GameConfigArgs { 
    //         opponent_id: Some(opponent()), 
    //         referrer_id: None 
    //     };
    //     let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
    //     let gc2 = GameConfigArgs { 
    //         opponent_id: Some(user()), 
    //         referrer_id: None 
    //     };
    //     let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
    //     make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
    //     make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        
    //     let game_id = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        
    //     let game = ctr.internal_get_game(&game_id);
    //     let player_1 = game.current_player_account_id().clone();
    //     let player_2 = game.next_player_account_id().clone();

    //     println!("( {} , {} )", player_1, player_2);

    //     assert_ne!(player_1, game.next_player_account_id());
    //     assert_ne!(game.players[0].piece, game.players[1].piece);
    //     assert_eq!(player_1, game.players[0].account_id);
    //     assert_eq!(player_2, game.players[1].account_id);
    //     assert_eq!(game.board.current_piece, game.players[0].piece);

    //     assert!(ctr.get_active_games().contains(&(game_id, game.clone())));

    //     let mut tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 0);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 0, 1);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 2);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 0);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 2, 1);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 2);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 1, 0);
    //     print_tiles(&tiles);
    //     testing_env!(ctx
    //         .predecessor_account_id(player_2.clone())
    //         .block_timestamp(sec_to_nano(601))
    //         .build()
    //     );
    //     // player2 turn too slow
    //     ctr.make_move(&game_id, 1, 2);
    //     assert!(ctr.get_stats(&player_1).victories_num == 1);
    //     assert!(ctr.get_stats(&player_2).victories_num == 0);
    //     assert_eq!(
    //         ctr.get_stats(&player_1).total_reward,
    //         Vec::from([
    //             (
    //                 acc_cheddar(),
    //                 (2 * ONE_CHEDDAR - (2 * ONE_CHEDDAR / BASIS_P as u128 * MIN_FEES as u128)) 
    //             )
    //         ])
    //     )
    // }

    // #[test]
    // fn test_stop_game() {
    //     let (mut ctx, mut ctr) = setup_contract(user(), Some(MIN_FEES), None,  None);
    //     whitelist_token(&mut ctr);
    //     assert_eq!(ctr.get_whitelisted_tokens(), Vec::from([
    //         (acc_cheddar(), (ONE_CHEDDAR / 10).into())
    //     ]));
    //     assert!(ctr.get_available_players().is_empty());
    //     let gc1 = GameConfigArgs { 
    //         opponent_id: Some(opponent()), 
    //         referrer_id: None 
    //     };
    //     let msg1 = near_sdk::serde_json::to_string(&gc1).expect("err serialize");
    //     let gc2 = GameConfigArgs { 
    //         opponent_id: Some(user()), 
    //         referrer_id: None 
    //     };
    //     let msg2 = near_sdk::serde_json::to_string(&gc2).expect("err serialize");
    //     make_available_ft(&mut ctx, &mut ctr, &user(), ONE_CHEDDAR, msg1);
    //     make_available_ft(&mut ctx, &mut ctr, &opponent(), ONE_CHEDDAR, msg2);
        
    //     let game_id = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        
    //     let game = ctr.internal_get_game(&game_id);
    //     let player_1 = game.current_player_account_id().clone();
    //     let player_2 = game.next_player_account_id().clone();

    //     println!("( {} , {} )", player_1, player_2);

    //     assert_ne!(player_1, game.next_player_account_id());
    //     assert_ne!(game.players[0].piece, game.players[1].piece);
    //     assert_eq!(player_1, game.players[0].account_id);
    //     assert_eq!(player_2, game.players[1].account_id);
    //     assert_eq!(game.board.current_piece, game.players[0].piece);

    //     assert!(ctr.get_active_games().contains(&(game_id, game.clone())));

    //     let mut tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 0);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 0, 1);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 0, 2);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 0);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 2, 1);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 2, 2);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_1, &game_id, 1, 0);
    //     print_tiles(&tiles);
    //     tiles = make_move(&mut ctx, &mut ctr, &player_2, &game_id, 1, 2);
    //     print_tiles(&tiles);
        
    //     stop_game(&mut ctx, &mut ctr, &player_2, &game_id, 601);
        
    //     let player_1_stats = ctr.get_stats(&opponent());
    //     let player_2_stats = ctr.get_stats(&user());

    //     assert!(
    //         player_1_stats.games_played == player_2_stats.games_played
    //     );
    //     assert!(
    //         player_2_stats.victories_num == 1 && player_1_stats.victories_num == 0
    //     );
    //     assert!(
    //         !player_2_stats.total_reward.is_empty() && player_1_stats.total_reward.is_empty()
    //     ); 
    //     assert_eq!(
    //         player_2_stats.total_reward,
    //         Vec::from([
    //             (
    //                 acc_cheddar(),
    //                 (2 * ONE_CHEDDAR - (2 * ONE_CHEDDAR / BASIS_P as u128 * MIN_FEES as u128)) 
    //             )
    //         ])
    //     )
    // }

    #[test]
    fn test_new_views() -> Result<(), std::io::Error>{
        let (mut ctx, mut ctr) = game_basics()?;

        println!("ContractParams: {:#?}", ctr.get_contract_params());
        println!("TotalStatsNum: {:#?}", ctr.get_total_stats_num());
        println!("AccountsPlayed: {:#?}", ctr.get_accounts_played());
        println!("UserPenalties: {:#?}", ctr.get_user_penalties(&user()));

        println!("PenaltyUsers: {:#?}", ctr.get_penalty_users());

        make_available_near(&mut ctx, &mut ctr, &user(), ONE_NEAR, None, None);
        make_available_near(&mut ctx, &mut ctr, &opponent(), ONE_NEAR, None, None);
        make_available_near(&mut ctx, &mut ctr, &"third".parse().unwrap(), ONE_NEAR, None, None);

        assert_eq!(ctr.get_available_players().len(), 3);

        testing_env!(ctx
            .block_timestamp(ctr.max_game_duration + MAX_TIME_TO_BE_AVAILABLE)
            .build()
        );
        assert_eq!(ctr.get_available_players().len(), 3);

        // test ping expired players
        testing_env!(ctx
            .block_timestamp(ctr.max_game_duration + MAX_TIME_TO_BE_AVAILABLE + 2)
            .build()
        );
        make_available_near(&mut ctx, &mut ctr, &"fourth".parse().unwrap(), ONE_NEAR, None, None);

        assert_eq!(ctr.get_available_players().len(), 1);
        assert_eq!(ctr.get_available_players()[0].0, "fourth".parse().unwrap());


        make_available_near(&mut ctx, &mut ctr, &user(), ONE_NEAR, None, None);
        make_available_near(&mut ctx, &mut ctr, &opponent(), ONE_NEAR, None, None);
        make_available_near(&mut ctx, &mut ctr, &"third".parse().unwrap(), ONE_NEAR, None, None);

        // first game starts at (max_game_duration + MAX_TIME_TO_BE_AVAILABLE +2) timestamp
        let first_game_id = start_game(&mut ctx, &mut ctr, &user(), &opponent());
        let first_game = ctr.internal_get_game(&first_game_id); 
        let current_player_first_game = first_game.current_player_account_id();
        let next_player_first_game = first_game.next_player_account_id();
        
        let future_winner_stats = ctr.get_stats(&next_player_first_game);
        let future_looser_stats = ctr.get_stats(&current_player_first_game);

        let winner_num_wins = future_winner_stats.victories_num;
        let looser_num_penalties = future_looser_stats.penalties_num;
        
        // second game starts 12 minutes after first
        testing_env!(ctx
            .block_timestamp(ctr.max_game_duration + MAX_TIME_TO_BE_AVAILABLE + 2 + ctr.max_turn_duration * 25 + 1)
            .build()
        );

        println!("game duration max - {}", ctr.max_game_duration);
        println!("turn duration max - {}", ctr.max_turn_duration);

        let second_game_id = start_game(&mut ctx, &mut ctr, &"third".parse().unwrap(), &"fourth".parse().unwrap());
        
        assert_eq!(ctr.get_active_games().len(), 3);

        let mut second_game = ctr.internal_get_game(&second_game_id); 
        let current_player_second_game = second_game.current_player_account_id();
        let next_player_second_game = second_game.next_player_account_id();

        testing_env!(ctx
            .block_timestamp(second_game.initiated_at + ctr.max_turn_duration - 1)
            .build()
        );
        make_move(&mut ctx, &mut ctr, &current_player_second_game, &second_game_id, 0, 0);
        second_game = ctr.internal_get_game(&second_game_id); 

        testing_env!(ctx
            .block_timestamp(second_game.initiated_at + (ctr.max_turn_duration - 1) + (ctr.max_turn_duration - 1))
            .build()
        );
        make_move(&mut ctx, &mut ctr, &next_player_second_game, &second_game_id, 0, 1);

        assert_eq!(
            ctr.get_active_games().len(), 1, 
            "first and second games need to be removed (expired) after max_game_duration passed for this"
        );
        assert_eq!(
            ctr.get_active_games()[0].0, second_game_id,
            "first and second games needs to be removed (expired) after max_game_duration passed for this"
        );

        println!("ContractParams: {:#?}", ctr.get_contract_params());

        let new_future_winner_stats = ctr.get_stats(&next_player_first_game);
        let new_future_looser_stats = ctr.get_stats(&current_player_first_game);

        let new_winner_num_wins = new_future_winner_stats.victories_num;
        let new_looser_num_penalties = new_future_looser_stats.penalties_num;
        
        assert!(new_winner_num_wins - winner_num_wins == 1);
        assert!(new_looser_num_penalties - looser_num_penalties == 1);

        assert!(
            ctr.get_penalty_users()
                .iter()
                .map(|(acc, _)| acc.clone())
                .collect::<Vec<AccountId>>()
                .contains(&current_player_first_game)
        );

        Ok(())
    }
}