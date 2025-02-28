import * as BufferLayout from '@solana/buffer-layout';

import {encodeData, decodeData, InstructionType} from './instruction';
import * as Layout from './layout';
import {PublicKey} from './publickey';
import {SystemProgram} from './system-program';
import {SYSVAR_CLOCK_PUBKEY, SYSVAR_RENT_PUBKEY} from './sysvar';
import {Transaction, TransactionInstruction} from './transaction';
import {toBuffer} from './util/to-buffer';

/**
 * Vote account info
 */
export class VoteInit {
  nodePubkey: PublicKey;
  authorizedVoter: PublicKey;
  authorizedWithdrawer: PublicKey;
  commission: number; /** [0, 100] */

  constructor(
    nodePubkey: PublicKey,
    authorizedVoter: PublicKey,
    authorizedWithdrawer: PublicKey,
    commission: number,
  ) {
    this.nodePubkey = nodePubkey;
    this.authorizedVoter = authorizedVoter;
    this.authorizedWithdrawer = authorizedWithdrawer;
    this.commission = commission;
  }
}

/**
 * Create vote account transaction params
 */
export type CreateVoteAccountParams = {
  fromPubkey: PublicKey;
  votePubkey: PublicKey;
  voteInit: VoteInit;
  lamports: number;
};

/**
 * InitializeAccount instruction params
 */
export type InitializeAccountParams = {
  votePubkey: PublicKey;
  nodePubkey: PublicKey;
  voteInit: VoteInit;
};

/**
 * Withdraw from vote account transaction params
 */
export type WithdrawFromVoteAccountParams = {
  votePubkey: PublicKey;
  authorizedWithdrawerPubkey: PublicKey;
  lamports: number;
  toPubkey: PublicKey;
};

/**
 * Vote Instruction class
 */
export class VoteInstruction {
  /**
   * @internal
   */
  constructor() {}

  /**
   * Decode a vote instruction and retrieve the instruction type.
   */
  static decodeInstructionType(
    instruction: TransactionInstruction,
  ): VoteInstructionType {
    this.checkProgramId(instruction.programId);

    const instructionTypeLayout = BufferLayout.u32('instruction');
    const typeIndex = instructionTypeLayout.decode(instruction.data);

    let type: VoteInstructionType | undefined;
    for (const [ixType, layout] of Object.entries(VOTE_INSTRUCTION_LAYOUTS)) {
      if (layout.index == typeIndex) {
        type = ixType as VoteInstructionType;
        break;
      }
    }

    if (!type) {
      throw new Error('Instruction type incorrect; not a VoteInstruction');
    }

    return type;
  }

  /**
   * Decode an initialize vote instruction and retrieve the instruction params.
   */
  static decodeInitializeAccount(
    instruction: TransactionInstruction,
  ): InitializeAccountParams {
    this.checkProgramId(instruction.programId);
    this.checkKeyLength(instruction.keys, 4);

    const {voteInit} = decodeData(
      VOTE_INSTRUCTION_LAYOUTS.InitializeAccount,
      instruction.data,
    );

    return {
      votePubkey: instruction.keys[0].pubkey,
      nodePubkey: instruction.keys[3].pubkey,
      voteInit: new VoteInit(
        new PublicKey(voteInit.nodePubkey),
        new PublicKey(voteInit.authorizedVoter),
        new PublicKey(voteInit.authorizedWithdrawer),
        voteInit.commission,
      ),
    };
  }

  /**
   * Decode a withdraw instruction and retrieve the instruction params.
   */
  static decodeWithdraw(
    instruction: TransactionInstruction,
  ): WithdrawFromVoteAccountParams {
    this.checkProgramId(instruction.programId);
    this.checkKeyLength(instruction.keys, 3);

    const {lamports} = decodeData(
      VOTE_INSTRUCTION_LAYOUTS.Withdraw,
      instruction.data,
    );

    return {
      votePubkey: instruction.keys[0].pubkey,
      authorizedWithdrawerPubkey: instruction.keys[2].pubkey,
      lamports,
      toPubkey: instruction.keys[1].pubkey,
    };
  }

  /**
   * @internal
   */
  static checkProgramId(programId: PublicKey) {
    if (!programId.equals(VoteProgram.programId)) {
      throw new Error('invalid instruction; programId is not VoteProgram');
    }
  }

  /**
   * @internal
   */
  static checkKeyLength(keys: Array<any>, expectedLength: number) {
    if (keys.length < expectedLength) {
      throw new Error(
        `invalid instruction; found ${keys.length} keys, expected at least ${expectedLength}`,
      );
    }
  }
}

/**
 * An enumeration of valid VoteInstructionType's
 */
export type VoteInstructionType = 'InitializeAccount' | 'Withdraw';

const VOTE_INSTRUCTION_LAYOUTS: {
  [type in VoteInstructionType]: InstructionType;
} = Object.freeze({
  InitializeAccount: {
    index: 0,
    layout: BufferLayout.struct([
      BufferLayout.u32('instruction'),
      Layout.voteInit(),
    ]),
  },
  Withdraw: {
    index: 3,
    layout: BufferLayout.struct([
      BufferLayout.u32('instruction'),
      BufferLayout.ns64('lamports'),
    ]),
  },
});

/**
 * Factory class for transactions to interact with the Vote program
 */
export class VoteProgram {
  /**
   * @internal
   */
  constructor() {}

  /**
   * Public key that identifies the Vote program
   */
  static programId: PublicKey = new PublicKey(
    'Vote111111111111111111111111111111111111111',
  );

  /**
   * Max space of a Vote account
   *
   * This is generated from the solana-vote-program VoteState struct as
   * `VoteState::size_of()`:
   * https://docs.rs/solana-vote-program/1.9.5/solana_vote_program/vote_state/struct.VoteState.html#method.size_of
   */
  static space: number = 3731;

  /**
   * Generate an Initialize instruction.
   */
  static initializeAccount(
    params: InitializeAccountParams,
  ): TransactionInstruction {
    const {votePubkey, nodePubkey, voteInit} = params;
    const type = VOTE_INSTRUCTION_LAYOUTS.InitializeAccount;
    const data = encodeData(type, {
      voteInit: {
        nodePubkey: toBuffer(voteInit.nodePubkey.toBuffer()),
        authorizedVoter: toBuffer(voteInit.authorizedVoter.toBuffer()),
        authorizedWithdrawer: toBuffer(
          voteInit.authorizedWithdrawer.toBuffer(),
        ),
        commission: voteInit.commission,
      },
    });
    const instructionData = {
      keys: [
        {pubkey: votePubkey, isSigner: false, isWritable: true},
        {pubkey: SYSVAR_RENT_PUBKEY, isSigner: false, isWritable: false},
        {pubkey: SYSVAR_CLOCK_PUBKEY, isSigner: false, isWritable: false},
        {pubkey: nodePubkey, isSigner: true, isWritable: false},
      ],
      programId: this.programId,
      data,
    };
    return new TransactionInstruction(instructionData);
  }

  /**
   * Generate a transaction that creates a new Vote account.
   */
  static createAccount(params: CreateVoteAccountParams): Transaction {
    const transaction = new Transaction();
    transaction.add(
      SystemProgram.createAccount({
        fromPubkey: params.fromPubkey,
        newAccountPubkey: params.votePubkey,
        lamports: params.lamports,
        space: this.space,
        programId: this.programId,
      }),
    );

    return transaction.add(
      this.initializeAccount({
        votePubkey: params.votePubkey,
        nodePubkey: params.voteInit.nodePubkey,
        voteInit: params.voteInit,
      }),
    );
  }

  /**
   * Generate a transaction to withdraw from a Vote account.
   */
  static withdraw(params: WithdrawFromVoteAccountParams): Transaction {
    const {votePubkey, authorizedWithdrawerPubkey, lamports, toPubkey} = params;
    const type = VOTE_INSTRUCTION_LAYOUTS.Withdraw;
    const data = encodeData(type, {lamports});

    const keys = [
      {pubkey: votePubkey, isSigner: false, isWritable: true},
      {pubkey: toPubkey, isSigner: false, isWritable: true},
      {pubkey: authorizedWithdrawerPubkey, isSigner: true, isWritable: false},
    ];

    return new Transaction().add({
      keys,
      programId: this.programId,
      data,
    });
  }
}
