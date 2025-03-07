// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Signing mach-o binaries.
//!
//! This module contains code for signing mach-o binaries.

use {
    crate::{
        code_directory::{CodeDirectoryBlob, CodeSignatureFlags, ExecutableSegmentFlags},
        code_requirement::{CodeRequirementExpression, CodeRequirements, RequirementType},
        cryptography::Digest,
        embedded_signature::{
            Blob, BlobData, CodeSigningSlot, ConstraintsDerBlob, EntitlementsBlob,
            EntitlementsDerBlob, RequirementSetBlob,
        },
        embedded_signature_builder::EmbeddedSignatureBuilder,
        entitlements::plist_to_executable_segment_flags,
        error::AppleCodesignError,
        macho::{semver_to_macho_target_version, MachFile, MachOBinary},
        macho_universal::create_universal_macho,
        policy::derive_designated_requirements,
        signing_settings::{DesignatedRequirementMode, SettingsScope, SigningSettings},
    },
    goblin::mach::{
        constants::{SEG_LINKEDIT, SEG_PAGEZERO},
        load_command::{
            CommandVariant, LinkeditDataCommand, SegmentCommand32, SegmentCommand64,
            LC_CODE_SIGNATURE, SIZEOF_LINKEDIT_DATA_COMMAND,
        },
        parse_magic_and_ctx,
    },
    log::{debug, info, warn},
    scroll::{ctx::SizeWith, IOwrite},
    std::{borrow::Cow, cmp::Ordering, collections::HashMap, io::Write, path::Path},
};

/// Derive a new Mach-O binary with new signature data.
fn create_macho_with_signature(
    macho: &MachOBinary,
    signature_data: &[u8],
) -> Result<Vec<u8>, AppleCodesignError> {
    // This should have already been called. But we do it again out of paranoia.
    macho.check_signing_capability()?;

    // The assumption made by checking_signing_capability() is that signature data
    // is at the end of the __LINKEDIT segment. So the replacement segment is the
    // existing segment truncated at the signature start followed by the new signature
    // data.
    //
    // Code signature data is aligned on 16 byte boundary by Apple convention.
    //
    // Typically segment data is aligned on pages, which are multiples of 16 bytes. So
    // it doesn't matter if we align based on Mach-O file-level or __LINKEDIT
    // segment-level offsets: the end result is 16 byte alignment in both.

    let linkedit_data_before_signature = macho
        .linkedit_data_before_signature()
        .ok_or(AppleCodesignError::MissingLinkedit)?;

    let signature_file_offset = macho.code_limit_binary_offset()?;
    let remainder = (signature_file_offset % 16) as usize;
    let signature_padding_length = if remainder == 0 { 0 } else { 16 - remainder };

    let signature_file_offset = signature_file_offset + signature_padding_length as u64;

    let new_linkedit_segment_size =
        linkedit_data_before_signature.len() + signature_padding_length + signature_data.len();

    // `codesign` rounds up the segment's vmsize to the nearest 16kb boundary.
    // We emulate that behavior.
    let remainder = new_linkedit_segment_size % 16384;
    let new_linkedit_segment_vmsize = if remainder == 0 {
        new_linkedit_segment_size
    } else {
        new_linkedit_segment_size + 16384 - remainder
    };

    assert!(new_linkedit_segment_vmsize >= new_linkedit_segment_size);
    assert_eq!(new_linkedit_segment_vmsize % 16384, 0);

    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());

    // Mach-O data structures are variable endian. So use the endian defined
    // by the magic when writing.
    let ctx = parse_magic_and_ctx(macho.data, 0)?
        .1
        .expect("context should have been parsed before");

    // If there isn't a code signature presently, we'll need to introduce a load
    // command for it.
    let mut header = macho.macho.header;
    if macho.code_signature_load_command().is_none() {
        header.ncmds += 1;
        header.sizeofcmds += SIZEOF_LINKEDIT_DATA_COMMAND as u32;
    }

    cursor.iowrite_with(header, ctx)?;

    // Following the header are load commands. We need to update load commands
    // to reflect changes to the signature size and __LINKEDIT segment size.

    let mut seen_signature_load_command = false;

    for load_command in &macho.macho.load_commands {
        let original_command_data =
            &macho.data[load_command.offset..load_command.offset + load_command.command.cmdsize()];

        let written_len = match &load_command.command {
            CommandVariant::CodeSignature(command) => {
                seen_signature_load_command = true;

                let mut command = *command;
                command.dataoff = signature_file_offset as _;
                command.datasize = signature_data.len() as _;

                cursor.iowrite_with(command, ctx.le)?;

                LinkeditDataCommand::size_with(&ctx.le)
            }
            CommandVariant::Segment32(segment) => {
                let segment = match segment.name() {
                    Ok(SEG_LINKEDIT) => {
                        let mut segment = *segment;
                        segment.filesize = new_linkedit_segment_size as _;
                        segment.vmsize = new_linkedit_segment_vmsize as _;

                        segment
                    }
                    _ => *segment,
                };

                cursor.iowrite_with(segment, ctx.le)?;

                SegmentCommand32::size_with(&ctx.le)
            }
            CommandVariant::Segment64(segment) => {
                let segment = match segment.name() {
                    Ok(SEG_LINKEDIT) => {
                        let mut segment = *segment;
                        segment.filesize = new_linkedit_segment_size as _;
                        segment.vmsize = new_linkedit_segment_vmsize as _;

                        segment
                    }
                    _ => *segment,
                };

                cursor.iowrite_with(segment, ctx.le)?;

                SegmentCommand64::size_with(&ctx.le)
            }
            _ => {
                // Reflect the original bytes.
                cursor.write_all(original_command_data)?;
                original_command_data.len()
            }
        };

        // For the commands we mutated ourselves, there may be more data after the
        // load command header. Write it out if present.
        cursor.write_all(&original_command_data[written_len..])?;
    }

    // If we didn't see a signature load command, write one out now.
    // Note: we're assuming that there's enough space between the end of
    // the original load commands and the beginning of the first section.
    // All this intermediate data should be 0s and we shouldn't be
    // interfering with anything here. But you never know.
    // TODO validate the added load command doesn't overflow into a section
    // or otherwise clobber data in the binary.
    if !seen_signature_load_command {
        let command = LinkeditDataCommand {
            cmd: LC_CODE_SIGNATURE,
            cmdsize: SIZEOF_LINKEDIT_DATA_COMMAND as _,
            dataoff: signature_file_offset as _,
            datasize: signature_data.len() as _,
        };

        cursor.iowrite_with(command, ctx.le)?;
    }

    let mut wrote_non_empty_segment = false;

    // Write out segments, updating the __LINKEDIT segment when we encounter it.
    for segment in macho.segments_by_file_offset() {
        // The initial __PAGEZERO segment contains no data (it is the magic and load
        // commands) and overlaps with the __TEXT segment, so we ignore it.
        if matches!(segment.name(), Ok(SEG_PAGEZERO)) {
            continue;
        }

        match cursor.position().cmp(&segment.fileoff) {
            // Mach-O segments may have padding between them. In this case, copy these
            // bytes (presumably NULLs but that isn't guaranteed) to the output.
            Ordering::Less => {
                let padding = &macho.data[cursor.position() as usize..segment.fileoff as usize];
                debug!(
                    "copying {} bytes outside segment boundaries before segment {}",
                    padding.len(),
                    segment.name().unwrap_or("<unknown>")
                );
                cursor.write_all(padding)?;
            }

            // The __TEXT segment usually has .fileoff = 0, which has it overlapping with
            // already written data. Allow this special case through.
            Ordering::Greater if segment.fileoff == 0 => {}

            // The initial non-empty segment is special because it can overlap
            // we the already written load commands.
            //
            // Usually the first non-empty segment is __TEXT and its file start
            // offset is 0x0. But we've seen binaries in the wild where the
            // offset is > 0x0. As long as the current cursor is before the first
            // section data, there should be no data corruption and we're good.
            Ordering::Greater if !wrote_non_empty_segment => {}

            // The writer has overran into this segment. That means we screwed up on a
            // previous loop iteration.
            Ordering::Greater => {
                return Err(AppleCodesignError::MachOWrite(format!(
                    "Mach-O segment corruption: cursor at 0x{:x} but segment begins at 0x{:x} (please report this bug)",
                    cursor.position(),
                    segment.fileoff
                )));
            }
            Ordering::Equal => {}
        }

        match segment.name() {
            Ok(SEG_LINKEDIT) => {
                cursor.write_all(
                    macho
                        .linkedit_data_before_signature()
                        .expect("__LINKEDIT segment data should resolve"),
                )?;

                let padding = vec![0u8; signature_padding_length];
                cursor.write_all(&padding)?;

                assert_eq!(cursor.position(), signature_file_offset);
                assert_eq!(cursor.position() % 16, 0);
                cursor.write_all(signature_data)?;
            }
            _ => {
                // At least the __TEXT segment has .fileoff = 0, which has it
                // overlapping with already written data. So only write segment
                // data new to the writer.
                if segment.fileoff < cursor.position() {
                    if segment.data.is_empty() {
                        continue;
                    }
                    let remaining =
                        &segment.data[cursor.position() as usize..segment.filesize as usize];
                    cursor.write_all(remaining)?;
                } else {
                    cursor.write_all(segment.data)?;
                }
            }
        }

        wrote_non_empty_segment = true;
    }

    Ok(cursor.into_inner())
}

/// Write Mach-O file content to an output file.
pub fn write_macho_file(
    input_path: &Path,
    output_path: &Path,
    macho_data: &[u8],
) -> Result<(), AppleCodesignError> {
    // Read permissions first in case we overwrite the original file.
    let permissions = std::fs::metadata(input_path)?.permissions();

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    {
        let mut fh = std::fs::File::create(output_path)?;
        fh.write_all(macho_data)?;
    }

    std::fs::set_permissions(output_path, permissions)?;

    Ok(())
}

/// Mach-O binary signer.
///
/// This type provides a high-level interface for signing Mach-O binaries.
/// It handles parsing and rewriting Mach-O binaries and contains most of the
/// functionality for producing signatures for individual Mach-O binaries.
///
/// Signing of both single architecture and fat/universal binaries is supported.
///
/// # Circular Dependency
///
/// There is a circular dependency between the generation of the Code Directory
/// present in the embedded signature and the Mach-O binary. See the note
/// in [crate::specification] for the gory details. The tl;dr is the Mach-O
/// data up to the signature data needs to be digested. But that digested data
/// contains load commands that reference the signature data and its size, which
/// can't be known until the Code Directory, CMS blob, and SuperBlob are all
/// created.
///
/// Our solution to this problem is to estimate the size of the embedded
/// signature data and then pad the unused data will 0s.
pub struct MachOSigner<'data> {
    /// Parsed Mach-O binaries.
    machos: Vec<MachOBinary<'data>>,
}

impl<'data> MachOSigner<'data> {
    /// Construct a new instance from unparsed data representing a Mach-O binary.
    ///
    /// The data will be parsed as a Mach-O binary (either single arch or fat/universal)
    /// and validated that we are capable of signing it.
    pub fn new(macho_data: &'data [u8]) -> Result<Self, AppleCodesignError> {
        let machos = MachFile::parse(macho_data)?.into_iter().collect::<Vec<_>>();

        Ok(Self { machos })
    }

    /// Write signed Mach-O data to the given writer using signing settings.
    pub async fn write_signed_binary(
        &self,
        settings: &SigningSettings<'_>,
        writer: &mut impl Write,
    ) -> Result<(), AppleCodesignError> {
        // Implementing a true streaming writer requires calculating final sizes
        // of all binaries so fat header offsets and sizes can be written first. We take
        // the easy road and buffer individual Mach-O binaries internally.

        let mut binaries = Vec::new();
        for (index, original_macho) in self.machos.iter().enumerate() {
            info!("signing Mach-O binary at index {}", index);
            let settings =
                settings.as_universal_macho_settings(index, original_macho.macho.header.cputype());

            let signature_len =
                self.estimate_embedded_signature_size(original_macho, &settings)?;

            // Derive an intermediate Mach-O with placeholder NULLs for signature
            // data so Code Directory digests over the load commands are correct.
            let placeholder_signature_data = b"\0".repeat(signature_len);

            let intermediate_macho_data =
                create_macho_with_signature(original_macho, &placeholder_signature_data)?;

            // A nice side-effect of this is that it catches bugs if we write malformed Mach-O!
            let intermediate_macho = MachOBinary::parse(&intermediate_macho_data)?;

            let mut signature_data = self
                .create_superblob(&settings, &intermediate_macho)
                .await?;
            info!("total signature size: {} bytes", signature_data.len());

            // The Mach-O writer adjusts load commands based on the signature length. So pad
            // with NULLs to get to our placeholder length.
            match signature_data.len().cmp(&placeholder_signature_data.len()) {
                Ordering::Greater => {
                    return Err(AppleCodesignError::SignatureDataTooLarge);
                }
                Ordering::Equal => {}
                Ordering::Less => {
                    signature_data.extend_from_slice(
                        &b"\0".repeat(placeholder_signature_data.len() - signature_data.len()),
                    );
                }
            }

            let signed = create_macho_with_signature(&intermediate_macho, &signature_data)?;
            binaries.push(signed);
        }

        if binaries.len() > 1 {
            create_universal_macho(writer, binaries.iter().map(|x| x.as_slice()))?;
        } else {
            writer.write_all(&binaries[0])?;
        }

        Ok(())
    }

    /// Create data constituting the SuperBlob to be embedded in the `__LINKEDIT` segment.
    ///
    /// The superblob contains the code directory, any extra blobs, and an optional
    /// CMS structure containing a cryptographic signature.
    ///
    /// This takes an explicit Mach-O to operate on due to a circular dependency
    /// between writing out the Mach-O and digesting its content. See the note
    /// in [MachOSigner] for details.
    pub async fn create_superblob(
        &self,
        settings: &SigningSettings<'_>,
        macho: &MachOBinary<'_>,
    ) -> Result<Vec<u8>, AppleCodesignError> {
        let mut builder = EmbeddedSignatureBuilder::default();

        for (slot, blob) in self.create_special_blobs(settings, macho.is_executable())? {
            builder.add_blob(slot, blob)?;
        }

        let code_directory = self.create_code_directory(settings, macho)?;
        info!("code directory version: {}", code_directory.version);

        builder.add_code_directory(CodeSigningSlot::CodeDirectory, code_directory)?;

        if let Some(digests) = settings.extra_digests(SettingsScope::Main) {
            for digest_type in digests {
                // Since everything consults settings for the digest to use, just make a new settings
                // with a different digest.
                let mut alt_settings = settings.clone();
                alt_settings.set_digest_type(SettingsScope::Main, *digest_type);

                info!(
                    "adding alternative code directory using digest {:?}",
                    digest_type
                );
                let cd = self.create_code_directory(&alt_settings, macho)?;

                builder.add_alternative_code_directory(cd)?;
            }
        }

        if let Some((signing_key, signing_cert)) = settings.signing_key() {
            builder
                .create_cms_signature(
                    signing_key,
                    signing_cert,
                    settings.time_stamp_url(),
                    settings.certificate_chain().iter().cloned(),
                    settings.signing_time(),
                )
                .await?;
        } else {
            builder.create_empty_cms_signature()?;
        }

        builder.create_superblob()
    }

    /// Create the `CodeDirectory` for the current configuration.
    ///
    /// This takes an explicit Mach-O to operate on due to a circular dependency
    /// between writing out the Mach-O and digesting its content. See the note
    /// in [MachOSigner] for details.
    pub fn create_code_directory(
        &self,
        settings: &SigningSettings,
        macho: &MachOBinary,
    ) -> Result<CodeDirectoryBlob<'static>, AppleCodesignError> {
        // TODO support defining or filling in proper values for fields with
        // static values.

        let target = macho.find_targeting()?;

        if let Some(target) = &target {
            info!(
                "binary targets {} >= {} with SDK {}",
                target.platform, target.minimum_os_version, target.sdk_version,
            );
        }

        let mut flags = CodeSignatureFlags::empty();

        if let Some(additional) = settings.code_signature_flags(SettingsScope::Main) {
            info!(
                "adding code signature flags from signing settings: {:?}",
                additional
            );
            flags |= additional;
        }

        // The adhoc flag is set when there is no CMS signature.
        if settings.signing_key().is_none() {
            info!("creating ad-hoc signature");
            flags |= CodeSignatureFlags::ADHOC;
        } else if flags.contains(CodeSignatureFlags::ADHOC) {
            info!("removing ad-hoc code signature flag");
            flags -= CodeSignatureFlags::ADHOC;
        }

        // Remove linker signed flag because we're not a linker.
        if flags.contains(CodeSignatureFlags::LINKER_SIGNED) {
            info!("removing linker signed flag from code signature (we're not a linker)");
            flags -= CodeSignatureFlags::LINKER_SIGNED;
        }

        // Code limit fields hold the file offset at which code digests stop. This
        // is the file offset in the `__LINKEDIT` segment when the embedded signature
        // SuperBlob begins.
        let (code_limit, code_limit_64) = match macho.code_limit_binary_offset()? {
            x if x > u32::MAX as u64 => (0, Some(x)),
            x => (x as u32, None),
        };

        let platform = 0;
        let page_size = 4096u32;

        let (exec_seg_base, exec_seg_limit) = macho.executable_segment_boundary()?;
        let (exec_seg_base, exec_seg_limit) = (Some(exec_seg_base), Some(exec_seg_limit));

        // Executable segment flags are wonky.
        //
        // Foremost, these flags are only present if the Mach-O binary is an executable. So not
        // matter what the settings say, we don't set these flags unless the Mach-O file type
        // is proper.
        //
        // Executable segment flags are also derived from an associated entitlements plist.
        let exec_seg_flags = if macho.is_executable() {
            if let Some(entitlements) = settings.entitlements_plist(SettingsScope::Main) {
                let flags = plist_to_executable_segment_flags(entitlements);

                if !flags.is_empty() {
                    info!("entitlements imply executable segment flags: {:?}", flags);
                }

                Some(flags | ExecutableSegmentFlags::MAIN_BINARY)
            } else {
                Some(ExecutableSegmentFlags::MAIN_BINARY)
            }
        } else {
            None
        };

        // The runtime version is the SDK version from the targeting loader commands. Same
        // u32 with nibbles encoding the version.
        //
        // If the runtime code signature flag is set, we also need to set the runtime version
        // or else the activation of the hardened runtime is incomplete.

        // If the settings defines a runtime version override, use it.
        let runtime = match settings.runtime_version(SettingsScope::Main) {
            Some(version) => {
                info!(
                    "using hardened runtime version {} from signing settings",
                    version
                );
                Some(semver_to_macho_target_version(version))
            }
            None => None,
        };

        // If we still don't have a runtime but need one, derive from the target SDK.
        let runtime = if runtime.is_none() && flags.contains(CodeSignatureFlags::RUNTIME) {
            if let Some(target) = &target {
                info!(
                    "using hardened runtime version {} derived from SDK version",
                    target.sdk_version
                );
                Some(semver_to_macho_target_version(&target.sdk_version))
            } else {
                warn!("hardened runtime version required but unable to derive suitable version; signature will likely fail Apple checks");
                None
            }
        } else {
            runtime
        };

        let digest_type = settings.digest_type(SettingsScope::Main);

        let code_hashes = macho
            .code_digests(digest_type, page_size as _)?
            .into_iter()
            .map(|v| Digest { data: v.into() })
            .collect::<Vec<_>>();

        let mut special_hashes = HashMap::new();

        // There is no corresponding blob for the info plist data since it is provided
        // externally to the embedded signature.
        if let Some(data) = settings.info_plist_data(SettingsScope::Main) {
            special_hashes.insert(
                CodeSigningSlot::Info,
                Digest {
                    data: digest_type.digest_data(data)?.into(),
                },
            );
        }

        // There is no corresponding blob for resources data since it is provided
        // externally to the embedded signature.
        if let Some(data) = settings.code_resources_data(SettingsScope::Main) {
            special_hashes.insert(
                CodeSigningSlot::ResourceDir,
                Digest {
                    data: digest_type.digest_data(data)?.into(),
                }
                .to_owned(),
            );
        }

        let ident = Cow::Owned(
            settings
                .binary_identifier(SettingsScope::Main)
                .ok_or(AppleCodesignError::NoIdentifier)?
                .to_string(),
        );

        // Team should only be included when signing with an Apple signed
        // certificate. This logic is handled in [SigningSettings]. But emit
        // a warning if the constraint is violated.
        let team_name = settings.team_id().map(|x| Cow::Owned(x.to_string()));

        if team_name.is_some() && !settings.signing_certificate_apple_signed() {
            warn!("signing without an Apple signed certificate but signing settings contain a team name; signature varies from Apple's tooling");
        }

        let mut cd = CodeDirectoryBlob {
            flags,
            code_limit,
            digest_size: digest_type.hash_len()? as u8,
            digest_type,
            platform,
            page_size,
            code_limit_64,
            exec_seg_base,
            exec_seg_limit,
            exec_seg_flags,
            runtime,
            ident,
            team_name,
            code_digests: code_hashes,
            ..Default::default()
        };

        for (slot, digest) in special_hashes {
            cd.set_slot_digest(slot, digest)?;
        }

        cd.adjust_version(target);
        cd.clear_newer_fields();

        Ok(cd)
    }

    /// Create blobs that need to be written given the current configuration.
    ///
    /// This emits all blobs except `CodeDirectory` and `Signature`, which are
    /// special since they are derived from the blobs emitted here.
    ///
    /// The goal of this function is to emit data to facilitate the creation of
    /// a `CodeDirectory`, which requires hashing blobs.
    pub fn create_special_blobs(
        &self,
        settings: &SigningSettings,
        is_executable: bool,
    ) -> Result<Vec<(CodeSigningSlot, BlobData<'static>)>, AppleCodesignError> {
        let mut res = Vec::new();

        let mut requirements = CodeRequirements::default();

        match settings.designated_requirement(SettingsScope::Main) {
            DesignatedRequirementMode::Auto => {
                // If we are using an Apple-issued cert, this should automatically
                // derive appropriate designated requirements.
                if let Some((_, cert)) = settings.signing_key() {
                    info!("deriving code requirements from signing certificate");
                    let identifier = Some(
                        settings
                            .binary_identifier(SettingsScope::Main)
                            .ok_or(AppleCodesignError::NoIdentifier)?
                            .to_string(),
                    );

                    let expr = derive_designated_requirements(
                        cert,
                        settings.certificate_chain(),
                        identifier,
                    )?;
                    requirements.push(expr);
                }
            }
            DesignatedRequirementMode::Explicit(exprs) => {
                info!("using provided code requirements");
                for expr in exprs {
                    requirements.push(CodeRequirementExpression::from_bytes(expr)?.0);
                }
            }
        }

        // Always emit a RequirementSet blob, even if empty. Without it, validation fails
        // with `the sealed resource directory is invalid`.
        let mut blob = RequirementSetBlob::default();

        if !requirements.is_empty() {
            requirements.add_to_requirement_set(&mut blob, RequirementType::Designated)?;
        }

        res.push((CodeSigningSlot::RequirementSet, blob.into()));

        if let Some(entitlements) = settings.entitlements_xml(SettingsScope::Main)? {
            let blob = EntitlementsBlob::from_string(&entitlements);

            res.push((CodeSigningSlot::Entitlements, blob.into()));
        }

        // The DER encoded entitlements weren't always present in the signature. The feature
        // appears to have been introduced in macOS 10.14 and is the default behavior as of
        // macOS 12 "when signing for all platforms." `codesign` appears to add the DER
        // representation whenever entitlements are present, but only if the current binary is
        // an executable (.filetype == MH_EXECUTE).
        if is_executable {
            if let Some(value) = settings.entitlements_plist(SettingsScope::Main) {
                let blob = EntitlementsDerBlob::from_plist(value)?;

                res.push((CodeSigningSlot::EntitlementsDer, blob.into()));
            }
        }

        if let Some(constraints) = settings.launch_constraints_self(SettingsScope::Main) {
            let blob = ConstraintsDerBlob::from_encoded_constraints(constraints)?;
            res.push((CodeSigningSlot::LaunchConstraintsSelf, blob.into()));
        }

        if let Some(constraints) = settings.launch_constraints_parent(SettingsScope::Main) {
            let blob = ConstraintsDerBlob::from_encoded_constraints(constraints)?;
            res.push((CodeSigningSlot::LaunchConstraintsParent, blob.into()));
        }

        if let Some(constraints) = settings.launch_constraints_responsible(SettingsScope::Main) {
            let blob = ConstraintsDerBlob::from_encoded_constraints(constraints)?;
            res.push((
                CodeSigningSlot::LaunchConstraintsResponsibleProcess,
                blob.into(),
            ));
        }

        if let Some(constraints) = settings.library_constraints(SettingsScope::Main) {
            let blob = ConstraintsDerBlob::from_encoded_constraints(constraints)?;
            res.push((CodeSigningSlot::LibraryConstraints, blob.into()));
        }

        Ok(res)
    }

    /// Estimate the size in bytes of an embedded code signature.
    pub fn estimate_embedded_signature_size(
        &self,
        macho: &MachOBinary,
        settings: &SigningSettings,
    ) -> Result<usize, AppleCodesignError> {
        let code_directory_count = 1 + settings
            .extra_digests(SettingsScope::Main)
            .map(|x| x.len())
            .unwrap_or_default();

        // Assume the common data structures are 1024 bytes.
        let mut size = 1024 * code_directory_count;

        // Reserve room for the code digests, which are proportional to binary size.
        size += macho.code_digests_size(settings.digest_type(SettingsScope::Main), 4096)?;

        if let Some(digests) = settings.extra_digests(SettingsScope::Main) {
            for digest in digests {
                size += macho.code_digests_size(*digest, 4096)?;
            }
        }

        // Add in sizes of all encoded blobs, as many blobs are variable size.
        for (_, blob) in self.create_special_blobs(settings, true)? {
            size += blob.to_blob_bytes()?.len();
        }

        // Assume the CMS data will take a fixed size.
        size += 4096;

        // Long certificate chains could blow up the size. Account for those.
        for cert in settings.certificate_chain() {
            size += cert.constructed_data().len();
        }

        // Resize space for CMS timestamp token, if being generated.
        //
        // We used to actually call out to a remote server here and obtain a
        // placeholder token. But this seemed excessive, especially since we did
        // it on every signing operation.
        //
        // Apple's TSTs are ~4200 bytes in size. We approximately double that
        // to give us some buffer.
        if settings.time_stamp_url().is_some() {
            size += 8192;
        }

        // Align on 1k boundaries just because.
        size += 1024 - size % 1024;

        Ok(size)
    }
}
