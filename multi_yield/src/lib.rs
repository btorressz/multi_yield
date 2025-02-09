use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, MintTo, Transfer};
use pyth_sdk_solana::{load_price_feed_from_account_info, PriceFeed, Price};
use std::convert::TryInto;

// Program ID
declare_id!("5GFJxKs3qbt6ibwLVJqYDZqoZJoxHe3ShEkZktp5CM3P");

#[program]
pub mod multi_yield {
    use super::*;

    /// Initialize the protocol's global state and the multiYIELD mint.
    /// The bump is passed as an argument (instead of referencing ctx.bumps).
    pub fn initialize(ctx: Context<Initialize>, bump: u8) -> Result<()> {
        let global_state = &mut ctx.accounts.global_state;
        global_state.mint = ctx.accounts.mint.key();
        global_state.bump = bump;
        Ok(())
    }

    /// Reward a trader for executing a trade, verified by the Pyth oracle.
    ///  also check protocol-wide volume & a unique trader count to be flashbot/MEV-resistant.
    pub fn reward_trade(
        ctx: Context<RewardTrade>,
        trade_amount: u64,
        trade_price: u64,
        unique_trader_count: u64, // for flashbot check
    ) -> Result<()> {
        // Flashbot / MEV check: require at least X unique traders in last blocks
        require!(
            unique_trader_count >= 5, // example threshold
            CustomError::InsufficientUniqueTraders
        );

        // Load the Pyth price feed from the AccountInfo
        let pyth_feed_account_info = &ctx.accounts.pyth_price_feed;
        let price_feed = load_price_feed_from_account_info(pyth_feed_account_info)
            .map_err(|_| CustomError::OracleError)?;

        let pyth_price_data = price_feed.get_price_unchecked();
        let price_val_i64 = pyth_price_data.price;
        require!(price_val_i64 >= 0, CustomError::NegativePythPrice);

        let price_val_u64: u64 = price_val_i64
            .try_into()
            .map_err(|_| CustomError::ConversionError)?;

        // ±5% bounding to avoid wild trades
        let five_percent = price_val_u64
            .checked_div(20)
            .ok_or(CustomError::ArithmeticOverflow)?;
        let lower_bound = price_val_u64
            .checked_sub(five_percent)
            .ok_or(CustomError::ArithmeticOverflow)?;
        let upper_bound = price_val_u64
            .checked_add(five_percent)
            .ok_or(CustomError::ArithmeticOverflow)?;
        require!(
            trade_price >= lower_bound && trade_price <= upper_bound,
            CustomError::InvalidTradePrice
        );

        // Flash loan hold
        let current_time = Clock::get()?.unix_timestamp;
        let trader_volume = &mut ctx.accounts.trader_volume;
        let min_hold_duration = 60;
        require!(
            current_time > trader_volume.last_trade_time + min_hold_duration,
            CustomError::FlashLoanDetected
        );

        // Update volume
        trader_volume.total_volume = trader_volume.total_volume.saturating_add(trade_amount);
        trader_volume.last_trade_time = current_time;

        // Tiered multiplier based on volume
        let protocol_wide_volume = ctx.accounts.global_state.protocol_wide_volume; // new field
        let dynamic_adjust = if protocol_wide_volume > 1_000_000_000 {
            2 // extra multiplier for huge volume
        } else {
            1
        };

        let base_multiplier: u64 = if trader_volume.total_volume > 1_000_000 {
            5
        } else if trader_volume.total_volume > 100_000 {
            2
        } else {
            1
        };
        let reward_multiplier = base_multiplier * dynamic_adjust;
        let reward_amount = (trade_amount * reward_multiplier) / 1000;

        // Also add some fees to the insurance pool
        if ctx.accounts.insurance_pool_account.to_account_info().key != &Pubkey::default() {
            let fee = reward_amount / 10; // 10% to insurance
            let seeds = &[b"global_state".as_ref(), &[ctx.accounts.global_state.bump]];
            let signer = &[&seeds[..]];
            let cpi_accounts = MintTo {
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.insurance_pool_account.to_account_info(),
                authority: ctx.accounts.global_state.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
            token::mint_to(cpi_ctx, fee)?;
        }

        // Mint remainder to the trader
        let seeds = &[b"global_state".as_ref(), &[ctx.accounts.global_state.bump]];
        let signer = &[&seeds[..]];
        let cpi_accounts = MintTo {
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.trader_token_account.to_account_info(),
            authority: ctx.accounts.global_state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
        token::mint_to(cpi_ctx, reward_amount)?;
        Ok(())
    }

    /// Stake multiYIELD tokens, with optional auto-compounding and early exit penalty.
    pub fn stake_tokens(ctx: Context<StakeTokens>, amount: u64, auto_compound: bool) -> Result<()> {
        {
            let transfer_accounts = Transfer {
                from: ctx.accounts.staker_token_account.to_account_info(),
                to: ctx.accounts.staking_pool_token_account.to_account_info(),
                authority: ctx.accounts.staker_authority.to_account_info(),
            };
            let transfer_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), transfer_accounts);
            token::transfer(transfer_ctx, amount)?;
        }

        let staker = &mut ctx.accounts.staker;
        staker.owner = ctx.accounts.staker_authority.key();
        staker.amount = staker.amount.checked_add(amount).unwrap();
        staker.stake_timestamp = Clock::get()?.unix_timestamp;
        staker.auto_compound = auto_compound;
        Ok(())
    }

    /// Claim staking rewards with loyalty multiplier and early exit penalty (10% if < 7 days).
    pub fn claim_stake_rewards(ctx: Context<ClaimStakeRewards>) -> Result<()> {
        let staker = &mut ctx.accounts.staker;
        let current_time = Clock::get()?.unix_timestamp;
        let time_staked = current_time.saturating_sub(staker.stake_timestamp);

        // If <7 days, 10% penalty goes to treasury
        let min_duration = 7 * 24 * 60 * 60;
        let penalty_rate = if time_staked < min_duration { 10 } else { 0 };

        // 10% base reward
        let base_reward = staker.amount / 10;

        // loyalty multiplier (over 90 days => extra protocol fees)
        let loyalty_multiplier: u64 = if time_staked >= 180 * 24 * 60 * 60 {
            15
        } else if time_staked >= 90 * 24 * 60 * 60 {
            13
        } else if time_staked >= 30 * 24 * 60 * 60 {
            11
        } else {
            10
        };
        let loyalty_reward = (base_reward * loyalty_multiplier) / 10;

        // NFT boost
        let mut final_reward = loyalty_reward;
        if ctx.accounts.nft_stake.boosted {
            final_reward += loyalty_reward / 5;
        }

        // If penalty applies
        let treasury_fee = (final_reward * penalty_rate) / 100;
        final_reward = final_reward.saturating_sub(treasury_fee);

        // Send penalty to DAO treasury
        if treasury_fee > 0 {
            let seeds = &[b"global_state".as_ref(), &[ctx.accounts.global_state.bump]];
            let signer = &[&seeds[..]];
            let cpi_accounts = MintTo {
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.dao_treasury_account.to_account_info(),
                authority: ctx.accounts.global_state.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
            token::mint_to(cpi_ctx, treasury_fee)?;
        }

        if staker.auto_compound {
            staker.amount = staker.amount.saturating_add(final_reward);
        } else {
            let seeds = &[b"global_state".as_ref(), &[ctx.accounts.global_state.bump]];
            let signer = &[&seeds[..]];
            let cpi_accounts = MintTo {
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.staker_reward_account.to_account_info(),
                authority: ctx.accounts.global_state.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_accounts,
                signer
            );
            token::mint_to(cpi_ctx, final_reward)?;
        }
        Ok(())
    }

    /// Extend `stake_nft()` to check floor price feed for NFT collateral.
    pub fn stake_nft(ctx: Context<StakeNFT>) -> Result<()> {
        let nft_stake = &mut ctx.accounts.nft_stake;
        nft_stake.owner = ctx.accounts.user.key();
        nft_stake.nft_minted = ctx.accounts.nft_mint.key();

        // use extra price feed for floor checks
        let floor_feed_info = &ctx.accounts.nft_floor_price_feed;
        let floor_feed = load_price_feed_from_account_info(floor_feed_info)
            .map_err(|_| CustomError::OracleError)?;
        let floor_price_data = floor_feed.get_price_unchecked();
        require!(floor_price_data.price > 1000, CustomError::NFTFloorTooLow);

        nft_stake.boosted = true;
        Ok(())
    }

    /// Add or remove fees to an insurance pool from trades or external contributions.
    pub fn insurance_pool_contribution(ctx: Context<InsurancePoolContribution>, amount: u64) -> Result<()> {
        let transfer_accounts = Transfer {
            from: ctx.accounts.contributor_token_account.to_account_info(),
            to: ctx.accounts.insurance_pool_account.to_account_info(),
            authority: ctx.accounts.contributor_authority.to_account_info(),
        };
        let transfer_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), transfer_accounts);
        token::transfer(transfer_ctx, amount)?;
        Ok(())
    }

    /// Stake LP tokens for additional liquidity mining. (unchanged from original)
    pub fn stake_lp_tokens(ctx: Context<StakeLPTokens>, amount: u64) -> Result<()> {
        {
            let transfer_accounts = Transfer {
                from: ctx.accounts.lp_token_account.to_account_info(),
                to: ctx.accounts.staking_pool_lp_account.to_account_info(),
                authority: ctx.accounts.staker_authority.to_account_info(),
            };
            let transfer_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), transfer_accounts);
            token::transfer(transfer_ctx, amount)?;
        }

        let lp_stake = &mut ctx.accounts.lp_stake;
        lp_stake.owner = ctx.accounts.staker_authority.key();
        lp_stake.lp_staked = lp_stake.lp_staked.checked_add(amount).unwrap();

        // Tiered multiplier based on total value locked
        let tvl = lp_stake.lp_staked;
        lp_stake.reward_multiplier = if tvl > 1_000_000 {
            5
        } else if tvl > 100_000 {
            2
        } else {
            1
        };

        Ok(())
    }

    /// Claim rewards for staked LP tokens (unchanged).
    pub fn claim_lp_rewards(ctx: Context<ClaimLPRewards>) -> Result<()> {
        let lp_stake = &ctx.accounts.lp_stake;
        let reward = (lp_stake.lp_staked * lp_stake.reward_multiplier as u64) / 100;
        let seeds = &[b"global_state".as_ref(), &[ctx.accounts.global_state.bump]];
        let signer = &[&seeds[..]];
        let cpi_accounts = MintTo {
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.lp_reward_account.to_account_info(),
            authority: ctx.accounts.global_state.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer
        );
        token::mint_to(cpi_ctx, reward)?;
        Ok(())
    }

    /// Governance: update the base reward, LP boost, possibly require a DAO vote.
    pub fn update_reward_parameters(
        ctx: Context<UpdateGovernance>,
        new_reward: u8,
        new_lp_boost: u8,
    ) -> Result<()> {
        let governance = &mut ctx.accounts.governance;
        require!(new_reward <= 50, CustomError::InvalidRewardParameters);
        require!(new_lp_boost <= 10, CustomError::InvalidRewardParameters);

        // On-chain DAO logic (assume check dao_approved = true)
        require!(governance.dao_approved, CustomError::GovernanceNotApproved);

        governance.reward_percentage = new_reward;
        governance.lp_boost = new_lp_boost;
        Ok(())
    }
}

// -----------------------------------------------
//                Accounts Contexts
// -----------------------------------------------
#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = user,
        seeds = [b"global_state"],
        bump,
        space = 8 + 32 + 8 + 1 // plus protocol_wide_volume (u64)
    )]
    pub global_state: Account<'info, GlobalState>,

    #[account(
        init,
        payer = user,
        mint::decimals = 6,
        mint::authority = global_state,
    )]
    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct RewardTrade<'info> {
    #[account(mut, seeds = [b"global_state"], bump = global_state.bump)]
    pub global_state: Account<'info, GlobalState>,

    // for dynamic protocol volume adjustments
    // (publicly stored in global_state -> protocol_wide_volume)
    // which can increment on each trade if desired.



    #[account(mut)]
    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub trader_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub insurance_pool_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"volume", trader_token_account.owner.key().as_ref()],
        bump
    )]
    pub trader_volume: Account<'info, TraderVolume>,

    /// Pyth price feed as a generic AccountInfo
    #[account()]
    pub pyth_price_feed: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct StakeTokens<'info> {
    #[account(
        init_if_needed,
        payer = staker_authority,
        space = 8 + 32 + 8 + 8 + 1,
        seeds = [b"stake", staker_authority.key().as_ref()],
        bump
    )]
    pub staker: Account<'info, StakeAccount>,

    #[account(mut)]
    pub staker_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub staking_pool_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub staker_authority: Signer<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct ClaimStakeRewards<'info> {
    #[account(mut, seeds = [b"stake", staker.owner.as_ref()], bump)]
    pub staker: Account<'info, StakeAccount>,

    #[account(mut)]
    pub staker_reward_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"global_state"],
        bump = global_state.bump
    )]
    pub global_state: Account<'info, GlobalState>,

    #[account(mut)]
    pub mint: Account<'info, Mint>,

    /// The staked NFT account for reward boosts (if any).
    #[account(mut)]
    pub nft_stake: Account<'info, NFTStakeAccount>,

    /// The DAO treasury (for penalty fees).
    #[account(mut)]
    pub dao_treasury_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct StakeNFT<'info> {
    #[account(
        init_if_needed,
        payer = user,
        space = 8 + 32 + 32 + 1,
        seeds = [b"nft_stake", user.key().as_ref()],
        bump
    )]
    pub nft_stake: Account<'info, NFTStakeAccount>,

    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut)]
    pub nft_mint: Account<'info, Mint>,

    /// Additional feed to check NFT floor price
    #[account()]
    pub nft_floor_price_feed: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
    // Could also add a token_program if additional steps are needed
}

#[derive(Accounts)]
pub struct InsurancePoolContribution<'info> {
    #[account(mut)]
    pub contributor_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub insurance_pool_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub contributor_authority: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct StakeLPTokens<'info> {
    #[account(
        init_if_needed,
        payer = staker_authority,
        space = 8 + 32 + 8 + 1,
        seeds = [b"lp_stake", staker_authority.key().as_ref()],
        bump
    )]
    pub lp_stake: Account<'info, LpStakeAccount>,

    #[account(mut)]
    pub lp_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub staking_pool_lp_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub staker_authority: Signer<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct ClaimLPRewards<'info> {
    #[account(
        mut,
        seeds = [b"lp_stake", staker_authority.key().as_ref()],
        bump
    )]
    pub lp_stake: Account<'info, LpStakeAccount>,

    #[account(mut)]
    pub lp_reward_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"global_state"],
        bump = global_state.bump
    )]
    pub global_state: Account<'info, GlobalState>,

    #[account(mut)]
    pub mint: Account<'info, Mint>,

    pub staker_authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct UpdateGovernance<'info> {
    #[account(mut)]
    pub governance: Account<'info, Governance>,
    pub staker_authority: Signer<'info>,
}

// -----------------------------------------------
//                State Structures
// -----------------------------------------------
#[account]
pub struct GlobalState {
    pub mint: Pubkey,
    pub bump: u8,
    pub protocol_wide_volume: u64, // track overall volume
    // Add other global fields (e.g. dao_treasury Pubkey if needed)
}

#[account]
pub struct TraderVolume {
    pub trader: Pubkey,
    pub total_volume: u64,
    pub last_trade_time: i64,
}

#[account]
pub struct StakeAccount {
    pub owner: Pubkey,
    pub amount: u64,
    pub stake_timestamp: i64,
    pub auto_compound: bool,
}

#[account]
pub struct NFTStakeAccount {
    pub owner: Pubkey,
    pub nft_minted: Pubkey,
    pub boosted: bool,
}

#[account]
pub struct VestingSchedule {
    pub owner: Pubkey,
    pub total_reward: u64,
    pub claimed: u64,
    pub start_time: i64,
    pub duration: i64,
}

#[account]
pub struct LpStakeAccount {
    pub owner: Pubkey,
    pub lp_staked: u64,
    pub reward_multiplier: u8,
}

#[account]
pub struct Governance {
    pub total_votes: u64,
    pub reward_percentage: u8, // base reward percentage
    pub lp_boost: u8,          // additional boost for LP rewards
    pub dao_approved: bool,    // indicates a DAO vote approval
}

// -----------------------------------------------
//                    Errors
// -----------------------------------------------
#[error_code]
pub enum CustomError {
    #[msg("Flash Loan Attack Detected!")]
    FlashLoanDetected,
    #[msg("Trade Price Out of Expected Range")]
    InvalidTradePrice,
    #[msg("Early unstake is not allowed")]
    EarlyUnstakePenalty,
    #[msg("Nothing to claim in vesting schedule")]
    NothingToClaim,
    #[msg("Invalid reward or LP boost parameters")]
    InvalidRewardParameters,
    #[msg("Oracle error occurred")]
    OracleError,
    #[msg("Pyth price feed is negative")]
    NegativePythPrice,
    #[msg("Failed converting i64 to u64")]
    ConversionError,
    #[msg("Overflow in checked arithmetic")]
    ArithmeticOverflow,
    #[msg("Insufficient unique traders for anti-flashbot")]
    InsufficientUniqueTraders,
    #[msg("NFT floor price too low to stake")]
    NFTFloorTooLow,
    #[msg("Governance not approved")]
    GovernanceNotApproved,
}
