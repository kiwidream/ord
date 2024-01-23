use super::*;

pub(super) struct Batch {
  pub(super) commit_fee_rate: FeeRate,
  pub(super) destinations: Vec<Address>,
  pub(super) dry_run: bool,
  pub(super) inscriptions: Vec<Inscription>,
  pub(super) mode: Mode,
  pub(super) no_backup: bool,
  pub(super) no_limit: bool,
  pub(super) parent_info: Option<ParentInfo>,
  pub(super) postage: Amount,
  pub(super) reinscribe: bool,
  pub(super) reveal_fee_rate: FeeRate,
  pub(super) commit_satpoint: Option<SatPoint>,
  pub(super) reveal_satpoints: Vec<SatPoint>
}

impl Default for Batch {
  fn default() -> Self {
    Self {
      commit_fee_rate: 1.0.try_into().unwrap(),
      destinations: Vec::new(),
      dry_run: false,
      inscriptions: Vec::new(),
      mode: Mode::SharedOutput,
      no_backup: false,
      no_limit: false,
      parent_info: None,
      postage: Amount::from_sat(10_000),
      reinscribe: false,
      reveal_fee_rate: 1.0.try_into().unwrap(),
      commit_satpoint: None,
      reveal_satpoints: Vec::new()
    }
  }
}

impl Batch {
  pub(crate) fn inscribe(
    &self,
    chain: Chain,
    index: &Index,
    client: &Client,
    locked_utxos: &BTreeSet<OutPoint>,
    runic_utxos: BTreeSet<OutPoint>,
    utxos: &BTreeMap<OutPoint, Amount>,
  ) -> SubcommandResult {
    let wallet_inscriptions = index.get_inscriptions(utxos)?;

    let commit_tx_change = [
      get_change_address(client, chain)?,
      get_change_address(client, chain)?,
    ];

    let (commit_tx, reveal_tx, recovery_key_pair, total_fees) = self
      .create_batch_inscription_transactions(
        &index,
        wallet_inscriptions,
        chain,
        locked_utxos.clone(),
        runic_utxos,
        utxos.clone(),
        commit_tx_change,
      )?;

    let psbts = Psbts {
      commit_psbt: bitcoin::psbt::PartiallySignedTransaction {
        inputs: vec![Default::default(); commit_tx.input.len()],
        outputs: vec![Default::default(); commit_tx.output.len()],

        unsigned_tx: commit_tx.clone(),
        xpub: Default::default(),
        version: 0,
        proprietary: Default::default(),
        unknown: Default::default(),
      }
      .serialize_hex(),
      reveal_psbt: bitcoin::psbt::PartiallySignedTransaction {
        inputs: vec![Default::default(); reveal_tx.input.len()],
        outputs: vec![Default::default(); reveal_tx.output.len()],

        unsigned_tx: reveal_tx.clone(),
        xpub: Default::default(),
        version: 0,
        proprietary: Default::default(),
        unknown: Default::default(),
      }
      .serialize_hex(),
    };

    if self.dry_run {
      return Ok(Some(Box::new(self.output(
        commit_tx.txid(),
        reveal_tx.txid(),
        total_fees,
        self.inscriptions.clone(),
        psbts,
      ))));
    }

    let signed_commit_tx = client
      .sign_raw_transaction_with_wallet(&commit_tx, None, None)?
      .hex;

    let signed_reveal_tx = if self.parent_info.is_some() || self.reveal_satpoints.len() > 0 {
      client
        .sign_raw_transaction_with_wallet(
          &reveal_tx,
          Some(
            &commit_tx
              .output
              .iter()
              .enumerate()
              .map(|(vout, output)| SignRawTransactionInput {
                txid: commit_tx.txid(),
                vout: vout.try_into().unwrap(),
                script_pub_key: output.script_pubkey.clone(),
                redeem_script: None,
                amount: Some(Amount::from_sat(output.value)),
              })
              .collect::<Vec<SignRawTransactionInput>>(),
          ),
          None,
        )?
        .hex
    } else {
      consensus::encode::serialize(&reveal_tx)
    };

    if !self.no_backup {
      Self::backup_recovery_key(client, recovery_key_pair, chain.network())?;
    }

    println!("commit tx {:?}", commit_tx);
    let commit = client.send_raw_transaction(&signed_commit_tx)?;

    println!("reveal tx {:?}", reveal_tx);
    let reveal = match client.send_raw_transaction(&signed_reveal_tx) {
      Ok(txid) => txid,
      Err(err) => {
        return Err(anyhow!(
        "Failed to send reveal transaction: {err}\nCommit tx {commit} will be recovered once mined"
      ))
      }
    };

    Ok(Some(Box::new(self.output(
      commit,
      reveal,
      total_fees,
      self.inscriptions.clone(),
      psbts,
    ))))
  }

  fn output(
    &self,
    commit_txid: Txid,
    reveal_txid: Txid,
    total_fees: u64,
    inscriptions: Vec<Inscription>,
    psbts: Psbts,
  ) -> super::Output {
    let mut inscriptions_output = Vec::new();
    for index in 0..inscriptions.len() {
      let index = u32::try_from(index).unwrap();

      let vout = match self.mode {
        Mode::SharedOutput | Mode::SameSat => {
          if self.parent_info.is_some() {
            1
          } else {
            0
          }
        }
        Mode::SeparateOutputs => {
          if self.parent_info.is_some() {
            index + 1
          } else {
            index
          }
        }
      };

      let offset: u64 = match self.mode {
        Mode::SharedOutput => u64::from(index) * self.postage.to_sat(),
        Mode::SeparateOutputs | Mode::SameSat => 0,
      };

      inscriptions_output.push(InscriptionInfo {
        id: InscriptionId {
          txid: reveal_txid,
          index,
        },
        location: SatPoint {
          outpoint: OutPoint {
            txid: reveal_txid,
            vout,
          },
          offset,
        },
      });
    }

    super::Output {
      commit: commit_txid,
      reveal: reveal_txid,
      total_fees,
      parent: self.parent_info.clone().map(|info| info.id),
      psbts,
      inscriptions: inscriptions_output,
    }
  }

  pub(crate) fn create_batch_inscription_transactions(
    &self,
    index: &Index,
    wallet_inscriptions: BTreeMap<SatPoint, InscriptionId>,
    chain: Chain,
    locked_utxos: BTreeSet<OutPoint>,
    runic_utxos: BTreeSet<OutPoint>,
    mut utxos: BTreeMap<OutPoint, Amount>,
    change: [Address; 2],
  ) -> Result<(Transaction, Transaction, TweakedKeyPair, u64)> {
    if let Some(parent_info) = &self.parent_info {
      assert!(self
        .inscriptions
        .iter()
        .all(|inscription| inscription.parent().unwrap() == parent_info.id))
    }

    match self.mode {
      Mode::SameSat => assert_eq!(
        self.destinations.len(),
        1,
        "invariant: same-sat has only one destination"
      ),
      Mode::SeparateOutputs => assert_eq!(
        self.destinations.len(),
        self.inscriptions.len(),
        "invariant: destination addresses and number of inscriptions doesn't match"
      ),
      Mode::SharedOutput => assert_eq!(
        self.destinations.len(),
        1,
        "invariant: destination addresses and number of inscriptions doesn't match"
      ),
    }

    let mut used_outpoints =
      self.reveal_satpoints
          .iter()
          .map(|satpoint| satpoint.outpoint)
          .collect::<BTreeSet<OutPoint>>();

    let cardinal_satpoint = Self::find_cardinal_utxo(
      &Amount::from_sat(0),
      used_outpoints.clone(),
      &utxos,
      wallet_inscriptions.clone(),
      locked_utxos.clone(),
      runic_utxos.clone()
    );

    let commit_satpoint = if let Some(commit_satpoint) = self.commit_satpoint {
      commit_satpoint
    } else {
      cardinal_satpoint.unwrap()
    };

    let mut reinscription = false;

    for (inscribed_satpoint, inscription_id) in &wallet_inscriptions {
      if reinscription && self.reinscribe {
        continue;
      }

      if *inscribed_satpoint == commit_satpoint {
        reinscription = true;
        if self.reinscribe {
          continue;
        } else {
          return Err(anyhow!("sat at {} already inscribed", commit_satpoint));
        }
      }

      if inscribed_satpoint.outpoint == commit_satpoint.outpoint {
        return Err(anyhow!(
          "utxo {} already inscribed with inscription {inscription_id} on sat {inscribed_satpoint}",
          commit_satpoint.outpoint,
        ));
      }
    }

    if self.reinscribe && !reinscription {
      return Err(anyhow!(
        "reinscribe flag set but this would not be a reinscription"
      ));
    }

    let total_postage = match self.mode {
      Mode::SameSat => self.postage,
      Mode::SharedOutput | Mode::SeparateOutputs => {
        self.postage * u64::try_from(self.inscriptions.len()).unwrap()
      }
    };

    let mut reveal_inputs = vec![OutPoint::null()];
    let mut reveal_tx_outs = Vec::new();
    let mut reveal_outputs = self
      .destinations
      .iter()
      .map(|destination| TxOut {
        script_pubkey: destination.script_pubkey(),
        value: match self.mode {
          Mode::SeparateOutputs => self.postage.to_sat(),
          Mode::SharedOutput | Mode::SameSat => total_postage.to_sat(),
        },
      })
      .collect::<Vec<TxOut>>();

    if let Some(ParentInfo {
      location,
      id: _,
      destination,
      tx_out,
    }) = self.parent_info.clone()
    {
      reveal_inputs.insert(0, location.outpoint);
      reveal_outputs.insert(
        0,
        TxOut {
          script_pubkey: destination.script_pubkey(),
          value: tx_out.value,
        },
      );
      reveal_tx_outs.push(tx_out);
    } else {
      reveal_inputs = Vec::new();

      for (pos, &satpoint) in self.reveal_satpoints.iter().enumerate() {
        if let Ok(tx_out) = index.get_tx_out(satpoint) {
          reveal_tx_outs.push(tx_out.clone());
          reveal_outputs[pos].value = tx_out.value;
        }
        reveal_inputs.push(satpoint.outpoint);
      }
    }

    let commit_input = if self.parent_info.is_some() { 1 } else { 0 };

    let mut pointer: u64 = 0;
    let mut inscriptions = Vec::new();
    for (pos, _inscription) in self.inscriptions.iter().enumerate() {
      let mut inscription_mod = self.inscriptions[pos].clone();
      inscription_mod.pointer = Some(Inscription::pointer_value(pointer));
      inscriptions.push(inscription_mod);

      if pos == commit_input {
        pointer += TARGET_POSTAGE.to_sat();
      } else {
        pointer += reveal_tx_outs[pos].value;
      }
    }

    let Ok((
      key_pair,
      control_block,
      taproot_spend_info,
      reveal_script
    )) = self.build_reveal_script(inscriptions) else { bail!("Failed to build reveal script") };

    let commit_tx_address = Address::p2tr_tweaked(taproot_spend_info.output_key(), chain.network());

    let (_, reveal_fee) = Self::build_reveal_transaction(
      &control_block,
      self.reveal_fee_rate,
      reveal_inputs.clone(),
      commit_input,
      reveal_outputs.clone(),
      &reveal_script,
    );

    let unsigned_commit_tx = TransactionBuilder::new(
      commit_satpoint,
      wallet_inscriptions.clone(),
      utxos.clone(),
      locked_utxos.clone(),
      runic_utxos.clone(),
      commit_tx_address.clone(),
      change,
      self.commit_fee_rate,
      Target::ExactPostage(TARGET_POSTAGE),
    )
    .build_transaction()?;

    let (vout, commit_output) = unsigned_commit_tx
      .output
      .iter()
      .enumerate()
      .find(|(_vout, output)| output.script_pubkey == commit_tx_address.script_pubkey())
      .expect("should find sat commit/inscription output");

    reveal_inputs[commit_input] = OutPoint {
      txid: unsigned_commit_tx.txid(),
      vout: vout.try_into().unwrap(),
    };

    let commit_cardinal_input = unsigned_commit_tx.input.len() - 1;
    let commit_cardinal_outpoint = unsigned_commit_tx.input[commit_cardinal_input].previous_output;
    println!("commit_cardinal_outpoint {commit_cardinal_outpoint}");
    used_outpoints.insert(commit_cardinal_outpoint);

    if let Some(reveal_cardinal_satpoint) = Self::find_cardinal_utxo(
        &reveal_fee,
        used_outpoints,
        &utxos,
        wallet_inscriptions.clone(),
        locked_utxos.clone(),
        runic_utxos.clone()
      ) {
        println!("reveal_cardinal_satpoint {reveal_cardinal_satpoint}");
      if let Ok(reveal_cardinal_tx_out) = index.get_tx_out(reveal_cardinal_satpoint) {
        reveal_tx_outs.push(reveal_cardinal_tx_out.clone());
        reveal_inputs.push(reveal_cardinal_satpoint.outpoint);
        reveal_outputs.push(TxOut {
          script_pubkey: commit_tx_address.script_pubkey(),
          value: reveal_cardinal_tx_out.value
        });
      }
    }

    let reveal_cardinal_output = reveal_outputs.len() - 1;
    let reveal_cardinal_value = reveal_outputs[reveal_cardinal_output].value;

    reveal_outputs[reveal_cardinal_output].value = reveal_cardinal_value - reveal_fee.to_sat();
    reveal_outputs[commit_input].value = commit_output.value;

    /* DEBUG */
    let out_val = reveal_outputs[reveal_cardinal_output].value;
    println!("reveal_cardinal_value {reveal_cardinal_value}");
    println!("reveal_fee {reveal_fee}");
    println!("out_val {out_val}");

    let (mut reveal_tx, _fee) = Self::build_reveal_transaction(
      &control_block,
      self.reveal_fee_rate,
      reveal_inputs,
      commit_input,
      reveal_outputs.clone(),
      &reveal_script,
    );

    if reveal_tx.output[commit_input].value
      < reveal_tx.output[commit_input]
        .script_pubkey
        .dust_value()
        .to_sat()
    {
      bail!("commit transaction output would be dust");
    }

    let mut prevouts = Vec::new();

    for tx_out in reveal_tx_outs {
      prevouts.push(tx_out);
    }

    let commit_tx_out = unsigned_commit_tx.output[vout].clone();

    if self.parent_info.is_some() {
      prevouts.push(commit_tx_out);
    } else {
      prevouts[commit_input] = commit_tx_out;
    }

    let mut sighash_cache = SighashCache::new(&mut reveal_tx);

    let sighash = sighash_cache
      .taproot_script_spend_signature_hash(
        commit_input,
        &Prevouts::All(&prevouts),
        TapLeafHash::from_script(&reveal_script, LeafVersion::TapScript),
        TapSighashType::Default,
      )
      .expect("signature hash should compute");

    let secp256k1 = Secp256k1::new();

    let sig = secp256k1.sign_schnorr(
      &secp256k1::Message::from_slice(sighash.as_ref())
        .expect("should be cryptographically secure hash"),
      &key_pair,
    );

    let witness = sighash_cache
      .witness_mut(commit_input)
      .expect("getting mutable witness reference should work");

    witness.push(
      Signature {
        sig,
        hash_ty: TapSighashType::Default,
      }
      .to_vec(),
    );

    witness.push(reveal_script);
    witness.push(&control_block.serialize());

    let recovery_key_pair = key_pair.tap_tweak(&secp256k1, taproot_spend_info.merkle_root());
    let (x_only_pub_key, _parity) = recovery_key_pair.to_inner().x_only_public_key();

    assert_eq!(
      Address::p2tr_tweaked(
        TweakedPublicKey::dangerous_assume_tweaked(x_only_pub_key),
        chain.network(),
      ),
      commit_tx_address
    );

    let reveal_weight = reveal_tx.weight();

    if !self.no_limit && reveal_weight > bitcoin::Weight::from_wu(MAX_STANDARD_TX_WEIGHT.into()) {
      bail!(
        "reveal transaction weight greater than {MAX_STANDARD_TX_WEIGHT} (MAX_STANDARD_TX_WEIGHT): {reveal_weight}"
      );
    }

    utxos.insert(
      reveal_tx.input[commit_input].previous_output,
      Amount::from_sat(
        unsigned_commit_tx.output[reveal_tx.input[commit_input].previous_output.vout as usize]
          .value,
      ),
    );

    let total_fees =
      Self::calculate_fee(&unsigned_commit_tx, &utxos) + Self::calculate_fee(&reveal_tx, &utxos);

    Ok((unsigned_commit_tx, reveal_tx, recovery_key_pair, total_fees))
  }

  fn backup_recovery_key(
    client: &Client,
    recovery_key_pair: TweakedKeyPair,
    network: Network,
  ) -> Result {
    let recovery_private_key = PrivateKey::new(recovery_key_pair.to_inner().secret_key(), network);

    let info = client.get_descriptor_info(&format!("rawtr({})", recovery_private_key.to_wif()))?;

    let response = client.import_descriptors(ImportDescriptors {
      descriptor: format!("rawtr({})#{}", recovery_private_key.to_wif(), info.checksum),
      timestamp: Timestamp::Now,
      active: Some(false),
      range: None,
      next_index: None,
      internal: Some(false),
      label: Some("commit tx recovery key".to_string()),
    })?;

    for result in response {
      if !result.success {
        return Err(anyhow!("commit tx recovery key import failed"));
      }
    }

    Ok(())
  }

  fn build_reveal_script(
    &self,
    inscriptions: Vec<Inscription>
  ) -> Result<(UntweakedKeyPair, ControlBlock, TaprootSpendInfo, ScriptBuf)> {
    let secp256k1 = Secp256k1::new();
    let key_pair = UntweakedKeyPair::new(&secp256k1, &mut rand::thread_rng());
    let (public_key, _parity) = XOnlyPublicKey::from_keypair(&key_pair);

    let reveal_script = Inscription::append_batch_reveal_script(
      &inscriptions,
      ScriptBuf::builder()
        .push_slice(public_key.serialize())
        .push_opcode(opcodes::all::OP_CHECKSIG),
    );

    let taproot_spend_info = TaprootBuilder::new()
      .add_leaf(0, reveal_script.clone())
      .expect("adding leaf should work")
      .finalize(&secp256k1, public_key)
      .expect("finalizing taproot builder should work");

    let control_block = taproot_spend_info
      .control_block(&(reveal_script.clone(), LeafVersion::TapScript))
      .expect("should compute control block");

    Ok((key_pair, control_block, taproot_spend_info, reveal_script))
  }

  fn build_reveal_transaction(
    control_block: &ControlBlock,
    fee_rate: FeeRate,
    inputs: Vec<OutPoint>,
    commit_input_index: usize,
    outputs: Vec<TxOut>,
    script: &Script,
  ) -> (Transaction, Amount) {
    let reveal_tx = Transaction {
      input: inputs
        .iter()
        .map(|outpoint| TxIn {
          previous_output: *outpoint,
          script_sig: script::Builder::new().into_script(),
          witness: Witness::new(),
          sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        })
        .collect(),
      output: outputs,
      lock_time: LockTime::ZERO,
      version: 2,
    };

    let fee = {
      let mut reveal_tx = reveal_tx.clone();

      for (current_index, txin) in reveal_tx.input.iter_mut().enumerate() {
        // add dummy inscription witness for reveal input/commit output
        if current_index == commit_input_index {
          txin.witness.push(
            Signature::from_slice(&[0; SCHNORR_SIGNATURE_SIZE])
              .unwrap()
              .to_vec(),
          );
          txin.witness.push(script);
          txin.witness.push(&control_block.serialize());
        } else {
          txin.witness = Witness::from_slice(&[&[0; SCHNORR_SIGNATURE_SIZE]]);
        }
      }

      fee_rate.fee(reveal_tx.vsize())
    };

    (reveal_tx, fee)
  }

  fn find_cardinal_utxo (
    min_amount: &Amount,
    used_outpoints: BTreeSet<OutPoint>,
    utxos: &BTreeMap<OutPoint, Amount>,
    wallet_inscriptions: BTreeMap<SatPoint, InscriptionId>,
    locked_utxos: BTreeSet<OutPoint>,
    runic_utxos: BTreeSet<OutPoint>,
  ) -> Option<SatPoint> {
    let inscribed_utxos = wallet_inscriptions
        .keys()
        .map(|satpoint| satpoint.outpoint)
        .collect::<BTreeSet<OutPoint>>();

    println!("used_outpoints {:?}", used_outpoints);

    return utxos
      .iter()
      .find(|(outpoint, amount)| {
        amount.to_sat() > min_amount.to_sat()
          && !inscribed_utxos.contains(outpoint)
          && !locked_utxos.contains(outpoint)
          && !runic_utxos.contains(outpoint)
          && !used_outpoints.contains(outpoint)
      })
      .map(|(outpoint, _amount)| SatPoint {
        outpoint: *outpoint,
        offset: 0,
      })
  }

  fn calculate_fee(tx: &Transaction, utxos: &BTreeMap<OutPoint, Amount>) -> u64 {
    tx.input
      .iter()
      .map(|txin| utxos.get(&txin.previous_output).unwrap().to_sat())
      .sum::<u64>()
      .checked_sub(tx.output.iter().map(|txout| txout.value).sum::<u64>())
      .unwrap()
  }
}

#[derive(PartialEq, Debug, Copy, Clone, Serialize, Deserialize, Default)]
pub(crate) enum Mode {
  #[serde(rename = "same-sat")]
  SameSat,
  #[default]
  #[serde(rename = "separate-outputs")]
  SeparateOutputs,
  #[serde(rename = "shared-output")]
  SharedOutput,
}

#[derive(Deserialize, Default, PartialEq, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchEntry {
  pub(crate) delegate: Option<InscriptionId>,
  pub(crate) destination: Option<Address<NetworkUnchecked>>,
  pub(crate) file: PathBuf,
  pub(crate) metadata: Option<serde_yaml::Value>,
  pub(crate) metaprotocol: Option<String>,
  pub(crate) satpoint: Option<SatPoint>
}

impl BatchEntry {
  pub(crate) fn metadata(&self) -> Result<Option<Vec<u8>>> {
    Ok(match &self.metadata {
      None => None,
      Some(metadata) => {
        let mut cbor = Vec::new();
        ciborium::into_writer(&metadata, &mut cbor)?;
        Some(cbor)
      }
    })
  }
}

#[derive(Deserialize, PartialEq, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct Batchfile {
  pub(crate) inscriptions: Vec<BatchEntry>,
  pub(crate) mode: Mode,
  pub(crate) parent: Option<InscriptionId>,
  pub(crate) postage: Option<u64>,
  pub(crate) sat: Option<Sat>
}

impl Batchfile {
  pub(crate) fn load(path: &Path) -> Result<Batchfile> {
    let batchfile: Batchfile = serde_yaml::from_reader(File::open(path)?)?;

    if batchfile.inscriptions.is_empty() {
      bail!("batchfile must contain at least one inscription");
    }

    Ok(batchfile)
  }

  pub(crate) fn inscriptions(
    &self,
    index: &Index,
    client: &Client,
    chain: Chain,
    parent_value: Option<u64>,
    metadata: Option<Vec<u8>>,
    postage: Amount,
    compress: bool,
  ) -> Result<(Vec<Inscription>, Vec<SatPoint>, Vec<Address>)> {
    assert!(!self.inscriptions.is_empty());

    if self
      .inscriptions
      .iter()
      .any(|entry| entry.destination.is_some())
      && self.mode == Mode::SharedOutput
    {
      return Err(anyhow!(
        "individual inscription destinations cannot be set in shared-output mode"
      ));
    }

    if metadata.is_some() {
      assert!(self
        .inscriptions
        .iter()
        .all(|entry| entry.metadata.is_none()));
    }

    let mut pointer = parent_value.unwrap_or_default();

    let mut inscriptions = Vec::new();
    let mut satpoints = Vec::new();
    for (i, entry) in self.inscriptions.iter().enumerate() {
      if let Some(delegate) = entry.delegate {
        ensure! {
          index.inscription_exists(delegate)?,
          "delegate {delegate} does not exist"
        }
      }

      inscriptions.push(Inscription::from_file(
        chain,
        compress,
        entry.delegate,
        match &metadata {
          Some(metadata) => Some(metadata.clone()),
          None => entry.metadata()?,
        },
        entry.metaprotocol.clone(),
        self.parent,
        &entry.file,
        if i == 0 { None } else { Some(pointer) },
      )?);

      if let Some(satpoint) = entry.satpoint {
        let tx_out = index.get_tx_out(satpoint)?;
        pointer += tx_out.value;
        satpoints.push(satpoint);
      } else {
        pointer += postage.to_sat();
      }
    }

    let destinations = match self.mode {
      Mode::SharedOutput | Mode::SameSat => vec![get_change_address(client, chain)?],
      Mode::SeparateOutputs => self
        .inscriptions
        .iter()
        .map(|entry| {
          entry.destination.as_ref().map_or_else(
            || get_change_address(client, chain),
            |address| {
              address
                .clone()
                .require_network(chain.network())
                .map_err(|e| e.into())
            },
          )
        })
        .collect::<Result<Vec<_>, _>>()?,
    };

    Ok((inscriptions, satpoints, destinations))
  }
}
