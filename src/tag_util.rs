/*
 Copyright (c) 2023 clone206

 This file is part of rdsd2pcm

 rdsd2pcm is free software: you can redistribute it and/or modify it
 under the terms of the GNU General Public License as published by the
 Free Software Foundation, either version 3 of the License, or
 (at your option) any later version.

 rdsd2pcm is distributed in the hope that it will be useful, but
 WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.
 You should have received a copy of the GNU General Public License
 along with rdsd2pcm. If not, see <https://www.gnu.org/licenses/>.
*/

use std::error::Error;
use std::io::Read;
use std::path::Path;

use flac_codec::metadata::{self, Picture, PictureType};
use id3::TagLike;
use log::debug;

/// Direct ID3 text-frame -> Vorbis comment field mappings (one-to-one).
/// Frames with special parsing (TRCK, TPOS, TDRC, COMM, USLT, TIPL, TMCL,
/// UFID, W-frames, TXXX) are handled separately in `id3_to_flac_meta`.
const ID3_VORBIS_MAP: &[(&str, &str)] = &[
    ("TPE1", "ARTIST"),
    ("TPE2", "ALBUMARTIST"),
    ("TSO2", "ALBUMARTISTSORT"),
    ("TSOA", "ALBUMSORT"),
    ("TSOP", "ARTISTSORT"),
    ("TSOT", "TITLESORT"),
    ("TIT2", "TITLE"),
    ("TALB", "ALBUM"),
    ("TCOM", "COMPOSER"),
    ("TCOP", "COPYRIGHT"),
    ("TCON", "GENRE"),
    ("TIT1", "GROUPING"),
    ("TSRC", "ISRC"),
    ("TPUB", "LABEL"),
    ("TEXT", "LYRICIST"),
    ("TSSE", "ENCODER"),
    ("TENC", "ENCODER"),
    ("TPE3", "CONDUCTOR"),
    ("TPE4", "REMIXER"),
    ("TMOO", "MOOD"),
    ("TKEY", "KEY"),
    ("TLAN", "LANGUAGE"),
    ("TMED", "MEDIA"),
    ("TOFN", "ORIGINALFILENAME"),
    ("TDRL", "RELEASEDATE"),
];

/// TIPL/IPLS role string -> Vorbis field name.
const ID3_TIPL_VORBIS_MAP: &[(&str, &str)] = &[
    ("arranger", "ARRANGER"),
    ("engineer", "ENGINEER"),
    ("dj-mix", "DJMIXER"),
    ("mix", "MIXER"),
    ("producer", "PRODUCER"),
];

/// Direct ID3 text-frame -> Vorbis comment field mappings (one-to-one).
/// Frames with special parsing (TRCK, TPOS, TDRC, COMM, USLT, TIPL, TMCL,
/// UFID, W-frames, TXXX) are handled separately in `id3_to_vorbis`.
const ID3_TXXX_VORBIS_MAP: &[(&str, &str)] = &[
    ("REPLAYGAIN_TRACK_GAIN", "REPLAYGAIN_TRACK_GAIN"),
    ("REPLAYGAIN_ALBUM_GAIN", "REPLAYGAIN_ALBUM_GAIN"),
    ("REPLAYGAIN_TRACK_PEAK", "REPLAYGAIN_TRACK_PEAK"),
    ("REPLAYGAIN_ALBUM_PEAK", "REPLAYGAIN_ALBUM_PEAK"),
    ("ASIN", "ASIN"),
    ("BARCODE", "BARCODE"),
    ("CATALOGNUMBER", "CATALOGNUMBER"),
];

/// Convert ID3 tag to VorbisComment metadata, following the
/// same field mapping used by MusicBrainz Picard.
pub fn id3_to_vorbis(
    tag: &id3::Tag,
) -> (metadata::VorbisComment, Vec<Picture>) {
    let unix_datetime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let mut vorbis = metadata::VorbisComment {
        vendor_string: format!(
            "dsd2dxd v{} Unix datetime {}",
            env!("CARGO_PKG_VERSION"),
            unix_datetime
        ),
        fields: Vec::new(),
    };

    // One-to-one text frame mappings via ID3_VORBIS_MAP
    for &(id3_id, vorbis_name) in ID3_VORBIS_MAP {
        if let Some(v) = tag.get(id3_id).and_then(|f| f.content().text()) {
            for val in v.split('\0').filter(|s| !s.is_empty()) {
                vorbis.insert(vorbis_name, val);
            }
        }
    }

    // TRCK -> TRACKNUMBER + TOTALTRACKS/TRACKTOTAL (parsed from "n/total")
    if let Some(n) = tag.track() {
        vorbis.insert("TRACKNUMBER", &n.to_string());
    }
    if let Some(n) = tag.total_tracks() {
        let s = n.to_string();
        vorbis.insert("TOTALTRACKS", &s);
        vorbis.insert("TRACKTOTAL", &s);
    }

    // TPOS -> DISCNUMBER + TOTALDISCS/DISCTOTAL (parsed from "n/total")
    if let Some(n) = tag.disc() {
        vorbis.insert("DISCNUMBER", &n.to_string());
    }
    if let Some(n) = tag.total_discs() {
        let s = n.to_string();
        vorbis.insert("TOTALDISCS", &s);
        vorbis.insert("DISCTOTAL", &s);
    }

    // TDRC -> DATE (full ISO timestamp); fall back to TYER year
    if let Some(date) = tag.date_recorded() {
        vorbis.insert("DATE", &date.to_string());
    } else if let Some(year) = tag.year() {
        vorbis.insert("DATE", &year.to_string());
    }

    // TDOR -> ORIGINALDATE; fall back to TORY (ID3v2.3 equivalent)
    if let Some(v) = tag
        .get("TDOR")
        .or_else(|| tag.get("TORY"))
        .and_then(|f| f.content().text())
    {
        vorbis.insert("ORIGINALDATE", v);
    }

    // MVIN -> MOVEMENTTOTAL + MOVEMENT (two Vorbis fields, one frame)
    if let Some(v) = tag.get("MVIN").and_then(|f| f.content().text()) {
        vorbis.insert("MOVEMENTTOTAL", v);
        vorbis.insert("MOVEMENT", v);
    }

    // COMM -> COMMENT
    if let Some(f) = tag.get("COMM") {
        if let id3::Content::Comment(comm) = f.content() {
            vorbis.insert("COMMENT", &comm.text);
        }
    }

    // USLT -> LYRICS
    if let Some(f) = tag.get("USLT") {
        if let id3::Content::Lyrics(lyrics) = f.content() {
            vorbis.insert("LYRICS", &lyrics.text);
        }
    }

    // TIPL/IPLS -> role-based fields via ID3_TIPL_VORBIS_MAP
    for frame in tag
        .frames()
        .filter(|f| f.id() == "TIPL" || f.id() == "IPLS")
    {
        if let id3::Content::InvolvedPeopleList(ip) = frame.content() {
            for item in &ip.items {
                for &(r, vorbis_name) in ID3_TIPL_VORBIS_MAP {
                    if item.involvement.eq_ignore_ascii_case(r) {
                        vorbis.insert(vorbis_name, &item.involvee);
                    }
                }
            }
        }
    }

    // TMCL -> PERFORMER "artist (instrument)"
    for frame in tag.frames().filter(|f| f.id() == "TMCL") {
        if let id3::Content::InvolvedPeopleList(mc) = frame.content() {
            for item in &mc.items {
                vorbis.insert(
                    "PERFORMER",
                    &format!("{} ({})", item.involvee, item.involvement),
                );
            }
        }
    }

    // UFID:http://musicbrainz.org -> MUSICBRAINZ_TRACKID
    for frame in tag.frames().filter(|f| f.id() == "UFID") {
        if let id3::Content::UniqueFileIdentifier(ufid) = frame.content() {
            if ufid.owner_identifier == "http://musicbrainz.org" {
                if let Ok(mbid) = std::str::from_utf8(&ufid.identifier) {
                    vorbis.insert("MUSICBRAINZ_TRACKID", mbid);
                }
            }
        }
    }

    // WCOP -> LICENSE, WOAR -> WEBSITE (web URL frames)
    for (frame_id, vorbis_name) in
        [("WCOP", "LICENSE"), ("WOAR", "WEBSITE")]
    {
        if let Some(f) = tag.get(frame_id) {
            if let id3::Content::Link(url) = f.content() {
                vorbis.insert(vorbis_name, url);
            }
        }
    }

    // TXXX -> use ID3_TXXX_VORBIS_MAP for known descriptions that need
    // renaming; fall back to the description as-is for everything else
    // (ReplayGain, ASIN, BARCODE, CATALOGNUMBER, etc.).
    for et in tag.extended_texts() {
        if et.description.is_empty() {
            continue;
        }
        let vorbis_name = ID3_TXXX_VORBIS_MAP
            .iter()
            .find(|(desc, _)| desc.eq_ignore_ascii_case(&et.description))
            .map(|&(_, name)| name)
            .unwrap_or(&et.description);
        for val in et.value.split('\0').filter(|s| !s.is_empty()) {
            vorbis.insert(vorbis_name, val);
        }
    }

    let mut pictures = Vec::new();

    for pic in tag.pictures() {
        let pic_type: PictureType = if pic.picture_type
            == id3::frame::PictureType::CoverFront
        {
            flac_codec::metadata::PictureType::FrontCover
        } else if pic.picture_type == id3::frame::PictureType::CoverBack {
            flac_codec::metadata::PictureType::BackCover
        } else {
            continue;
        };
        debug!("Adding ID3 Picture: {}", pic);
        let picture = flac_codec::metadata::Picture::new(
            pic_type,
            pic.description.clone(),
            pic.data.clone(),
        );
        if let Ok(my_pic) = picture {
            pictures.push(my_pic);
        }
    }
    (vorbis, pictures)
}

/// Write `tag` as an ID3v2 chunk into the WAV file at `out_path`
///
/// The `id3` crate (as of 1.16.3) detects a WAV file solely by its
/// `RIFF`/`WAVE` magic bytes (see its `Format::magic`);
pub fn write_wav_id3_tag(
    out_path: &Path,
    tag: &id3::Tag,
    version: id3::Version,
) -> Result<(), Box<dyn Error>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(out_path)
        .map_err(|e| format!("WAV id3 open: {e}"))?;

    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .map_err(|e| format!("WAV id3 read magic: {e}"))?;

    drop(file);
    tag.write_to_path(out_path, version)?;
    return Ok(());
}
