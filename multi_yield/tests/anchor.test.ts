// No imports needed: web3, BN, pg, and assert are globally available
//edi ttests

describe("multi_yield Tests", () => {
  let globalStatePda;
  let globalStateBump;

  let traderVolumePda;
  let traderVolumeBump;

  let stakerPda;
  let stakerBump;

  // Example placeholders for addresses. 
  let mintPubkey;
  let insurancePoolAccount;
  let traderTokenAccount;
  let stakerTokenAccount;
  let stakerRewardAccount;
  let daoTreasuryTokenAccount;
  let nftStakePda;

  before(async () => {
    //  Derive the globalState PDA
    [globalStatePda, globalStateBump] =
      await web3.PublicKey.findProgramAddress(
        [Buffer.from("global_state")],
        pg.program.programId // Use pg.program.programId
      );

    //  Derive the "volume" PDA for a traderVolume account
    [traderVolumePda, traderVolumeBump] =
      await web3.PublicKey.findProgramAddress(
        [Buffer.from("volume"), pg.wallet.publicKey.toBuffer()],
        pg.program.programId
      );

    //  Derive a "stake" PDA
    [stakerPda, stakerBump] = await web3.PublicKey.findProgramAddress(
      [Buffer.from("stake"), pg.wallet.publicKey.toBuffer()],
      pg.program.programId
    );

    //  Fill in actual addresses
     mintPubkey = new web3.PublicKey("...someMintPubkey...");
    insurancePoolAccount = new web3.PublicKey("...insurancePoolAccount...");
    traderTokenAccount = new web3.PublicKey("...traderTokenAccount...");
    stakerTokenAccount = new web3.PublicKey("...stakerTokenAccount...");
    stakerRewardAccount = new web3.PublicKey("...stakerRewardAccount...");
    daoTreasuryTokenAccount = new web3.PublicKey("...daoTreasuryTokenAccount...");
    nftStakePda = new web3.PublicKey("...nftStakePda...");
  });

  it("initialize", async () => {
    const txHash = await pg.program.methods
      .initialize(globalStateBump)
      .accounts({
        globalState: globalStatePda,
        mint: mintPubkey,
        user: pg.wallet.publicKey,
        systemProgram: web3.SystemProgram.programId, 
        tokenProgram: new web3.PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"), 
        rent: web3.SYSVAR_RENT_PUBKEY, 
      })
      .rpc();

    console.log("initialize() tx:", txHash);
    await pg.connection.confirmTransaction(txHash);

    // Check globalState
    const globalState = await pg.program.account.globalState.fetch(globalStatePda);
    console.log("GlobalState data:", globalState);
    assert.equal(globalState.bump, globalStateBump, "GlobalState bump mismatch");
  });

  it("rewardTrade", async () => {
    const tradeAmount = new BN(10_000);
    const tradePrice = new BN(1050);
    const uniqueTraderCount = new BN(5);

    const txHash = await pg.program.methods
      .rewardTrade(tradeAmount, tradePrice, uniqueTraderCount)
      .accounts({
        globalState: globalStatePda,
        mint: mintPubkey,
        traderTokenAccount: traderTokenAccount,
        insurancePoolAccount: insurancePoolAccount,
        traderVolume: traderVolumePda,
        pythPriceFeed: new web3.PublicKey("...pythPriceFeed..."),
        tokenProgram: new web3.PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"), 
      })
      .rpc();

    console.log("rewardTrade() tx:", txHash);
    await pg.connection.confirmTransaction(txHash);

    const traderVolume = await pg.program.account.traderVolume.fetch(traderVolumePda);
    console.log("TraderVolume data:", traderVolume);
    assert.equal(traderVolume.totalVolume.toString(), "10000", "trade_amount mismatch");
  });

  it("stakeTokens", async () => {
    const stakeAmount = new BN(5000);
    const autoCompound = true;

    const txHash = await pg.program.methods
      .stakeTokens(stakeAmount, autoCompound)
      .accounts({
        staker: stakerPda,
        stakerTokenAccount: stakerTokenAccount,
        stakingPoolTokenAccount: new web3.PublicKey("...stakingPoolAcct..."),
        stakerAuthority: pg.wallet.publicKey,
        tokenProgram: new web3.PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"), 
        systemProgram: web3.SystemProgram.programId, 
        rent: web3.SYSVAR_RENT_PUBKEY, 
      })
      .rpc();

    console.log("stakeTokens() tx:", txHash);
    await pg.connection.confirmTransaction(txHash);

    const stakerAccount = await pg.program.account.stakeAccount.fetch(stakerPda);
    console.log("StakerAccount data:", stakerAccount);
    assert.equal(stakerAccount.amount.toString(), "5000", "Staker amount mismatch");
  });

  it("claimStakeRewards", async () => {
    const txHash = await pg.program.methods
      .claimStakeRewards()
      .accounts({
        staker: stakerPda,
        stakerRewardAccount: stakerRewardAccount,
        globalState: globalStatePda,
        mint: mintPubkey,
        nftStake: nftStakePda,
        daoTreasuryAccount: daoTreasuryTokenAccount,
        tokenProgram: new web3.PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"), 
      })
      .rpc();

    console.log("claimStakeRewards() tx:", txHash);
    await pg.connection.confirmTransaction(txHash);

    const stakerAccount = await pg.program.account.stakeAccount.fetch(stakerPda);
    console.log("StakerAccount data after claim:", stakerAccount);
  });
});
