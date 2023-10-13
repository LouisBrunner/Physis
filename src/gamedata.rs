// SPDX-FileCopyrightText: 2023 Joshua Goins <josh@redstrate.com>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::fs;
use std::fs::{DirEntry, ReadDir};
use std::path::PathBuf;

use tracing::debug;

use crate::common::{Language, read_version};
use crate::dat::DatFile;
use crate::exd::EXD;
use crate::exh::EXH;
use crate::exl::EXL;
use crate::index::IndexFile;
use crate::ByteBuffer;
use crate::patch::{apply_patch, PatchError};
use crate::repository::{Category, Repository, string_to_category};
use crate::sqpack::calculate_hash;

/// Framework for operating on game data.
pub struct GameData {
    /// The game directory to operate on.
    pub game_directory: String,

    /// Repositories in the game directory.
    pub repositories: Vec<Repository>,

    index_files: HashMap<String, IndexFile>
}

fn is_valid(path: &str) -> bool {
    let d = PathBuf::from(path);

    if fs::metadata(d.as_path()).is_err() {
        println!("Failed game directory.");
        return false;
    }

    true
}

#[derive(Debug)]
pub enum RepairAction {
    VersionFileMissing,
    VersionFileCanRestore,
}

#[derive(Debug)]
pub enum RepairError<'a> {
    FailedRepair(&'a Repository),
}

impl GameData {
    /// Read game data from an existing game installation.
    ///
    /// This will return _None_ if the game directory is not valid, but it does not check the validity
    /// of each individual file.
    ///
    /// **Note**: None of the repositories are searched, and it's required to call `reload_repositories()`.
    ///
    /// # Example
    ///
    /// ```
    /// # use physis::gamedata::GameData;
    /// GameData::from_existing("$FFXIV/game");
    /// ```
    pub fn from_existing(directory: &str) -> Option<GameData> {
        debug!(directory, "Loading game directory");

        match is_valid(directory) {
            true => Some(Self {
                game_directory: String::from(directory),
                repositories: vec![],
                index_files: HashMap::new()
            }),
            false => {
                println!("Game data is not valid!");
                None
            }
        }
    }

    /// Reloads all repository information from disk. This is a fast operation, as it's not actually
    /// reading any dat files yet.
    ///
    /// # Example
    ///
    /// ```should_panic
    /// # use physis::gamedata::GameData;
    /// let mut game = GameData::from_existing("$FFXIV/game").unwrap();
    /// game.reload_repositories();
    /// ```
    pub fn reload_repositories(&mut self) {
        self.repositories.clear();

        let mut d = PathBuf::from(self.game_directory.as_str());

        // add initial ffxiv directory
        if let Some(base_repository) = Repository::from_existing_base(d.to_str().unwrap()) {
            self.repositories.push(base_repository);
        }

        // add expansions
        d.push("sqpack");

        if let Ok(repository_paths) = fs::read_dir(d.as_path()) {
            let repository_paths : ReadDir = repository_paths;

            let repository_paths : Vec<DirEntry> = repository_paths
                .filter_map(Result::ok)
                .filter(|s| s.file_type().unwrap().is_dir())
                .collect();

            for repository_path in repository_paths {
                if let Some(expansion_repository) =Repository::from_existing(repository_path.path().to_str().unwrap()) {
                    self.repositories.push(expansion_repository);
                }
            }
        }

        self.repositories.sort();
    }

    fn get_dat_file(&self, path: &str, data_file_id: u32) -> Option<DatFile> {
        let (repository, category) = self.parse_repository_category(path).unwrap();

        let dat_path: PathBuf = [
            self.game_directory.clone(),
            "sqpack".to_string(),
            repository.name.clone(),
            repository.dat_filename(category, data_file_id),
        ]
        .iter()
        .collect();

        DatFile::from_existing(dat_path.to_str()?)
    }

    /// Checks if a file located at `path` exists.
    ///
    /// # Example
    ///
    /// ```should_panic
    /// # use physis::gamedata::GameData;
    /// # let mut game = GameData::from_existing("SquareEnix/Final Fantasy XIV - A Realm Reborn/game").unwrap();
    /// if game.exists("exd/cid.exl") {
    ///     println!("Cid really does exist!");
    /// } else {
    ///     println!("Oh noes!");
    /// }
    /// ```
    pub fn exists(&mut self, path: &str) -> bool {
        let hash = calculate_hash(path);
        let index_path = self.get_index_filename(path);

        self.cache_index_file(&index_path);
        let index_file = self
            .get_index_file(&index_path)
            .expect("Failed to find index file.");

        index_file.entries.iter().any(|s| s.hash == hash)
    }

    /// Extracts the file located at `path`. This is returned as an in-memory buffer, and will usually
    /// have to be further parsed.
    ///
    /// # Example
    ///
    /// ```should_panic
    /// # use physis::gamedata::GameData;
    /// # use std::io::Write;
    /// # let mut game = GameData::from_existing("SquareEnix/Final Fantasy XIV - A Realm Reborn/game").unwrap();
    /// let data = game.extract("exd/root.exl").unwrap();
    ///
    /// let mut file = std::fs::File::create("root.exl").unwrap();
    /// file.write(data.as_slice()).unwrap();
    /// ```
    pub fn extract(&mut self, path: &str) -> Option<ByteBuffer> {
        debug!(file=path, "Extracting file");

        let hash = calculate_hash(path);
        let index_path = self.get_index_filename(path);

        self.cache_index_file(&index_path);
        let index_file = self.get_index_file(&index_path)?;

        let slice = index_file.entries.iter().find(|s| s.hash == hash);
        match slice {
            Some(entry) => {
                let mut dat_file = self.get_dat_file(path, entry.bitfield.data_file_id().into())?;

                dat_file.read_from_offset(entry.bitfield.offset())
            }
            None => None,
        }
    }

    /// Parses a path structure and spits out the corresponding category and repository.
    fn parse_repository_category(&self, path: &str) -> Option<(&Repository, Category)> {
        let tokens: Vec<&str> = path.split('/').collect(); // TODO: use split_once here
        let repository_token = tokens[0];

        if tokens.len() < 2 {
            return None;
        }

        for repository in &self.repositories {
            if repository.name == repository_token {
                return Some((repository, string_to_category(tokens[1])?));
            }
        }

        Some((&self.repositories[0], string_to_category(tokens[0])?))
    }

    fn get_index_filename(&self, path: &str) -> String {
        let (repository, category) = self.parse_repository_category(path).unwrap();

        let index_path: PathBuf = [
            &self.game_directory,
            "sqpack",
            &repository.name,
            &repository.index_filename(category),
        ]
            .iter()
            .collect();

        index_path.into_os_string().into_string().unwrap()
    }

    pub fn read_excel_sheet_header(&mut self, name: &str) -> Option<EXH> {
        let root_exl_file = self.extract("exd/root.exl")?;

        let root_exl = EXL::from_existing(&root_exl_file)?;

        for (row, _) in root_exl.entries {
            if row == name {
                let new_filename = name.to_lowercase();

                let path = format!("exd/{new_filename}.exh");

                return EXH::from_existing(&self.extract(&path)?);
            }
        }

        None
    }

    pub fn get_all_sheet_names(&mut self) -> Option<Vec<String>> {
        let root_exl_file = self.extract("exd/root.exl")?;

        let root_exl = EXL::from_existing(&root_exl_file)?;

        let mut names = vec![];
        for (row, _) in root_exl.entries {
            names.push(row);
        }

        Some(names)
    }

    pub fn read_excel_sheet(
        &mut self,
        name: &str,
        exh: &EXH,
        language: Language,
        page: usize,
    ) -> Option<EXD> {
        let exd_path = format!(
            "exd/{}",
            EXD::calculate_filename(name, language, &exh.pages[page])
        );

        let exd_file = self.extract(&exd_path)?;

        EXD::from_existing(exh, &exd_file)
    }

    pub fn apply_patch(&self, patch_path: &str) -> Result<(), PatchError> {
        apply_patch(&self.game_directory, patch_path)
    }

    /// Detects whether or not the game files need a repair, right now it only checks for invalid
    /// version files.
    /// If the repair is needed, a list of invalid repositories is given.
    pub fn needs_repair(&self) -> Option<Vec<(&Repository, RepairAction)>> {
        let mut repositories: Vec<(&Repository, RepairAction)> = Vec::new();
        for repository in &self.repositories {
            if repository.version.is_none() {
                // Check to see if a .bck file is created, as we might be able to use that
                let ver_bak_path: PathBuf = [
                    self.game_directory.clone(),
                    "sqpack".to_string(),
                    repository.name.clone(),
                    format!("{}.bck", repository.name),
                ]
                .iter()
                .collect();

                let repair_action = if read_version(&ver_bak_path).is_some() {
                    RepairAction::VersionFileCanRestore
                } else {
                    RepairAction::VersionFileMissing
                };

                repositories.push((repository, repair_action));
            }
        }

        if repositories.is_empty() {
            None
        } else {
            Some(repositories)
        }
    }

    /// Performs the repair, assuming any damaging effects it may have
    /// Returns true only if all actions were taken are successful.
    /// NOTE: This is a destructive operation, especially for InvalidVersion errors.
    pub fn perform_repair<'a>(
        &self,
        repositories: &Vec<(&'a Repository, RepairAction)>,
    ) -> Result<(), RepairError<'a>> {
        for (repository, action) in repositories {
            let ver_path: PathBuf = [
                self.game_directory.clone(),
                "sqpack".to_string(),
                repository.name.clone(),
                format!("{}.ver", repository.name),
            ]
            .iter()
            .collect();

            let new_version: String = match action {
                RepairAction::VersionFileMissing => {
                    let repo_path: PathBuf = [
                        self.game_directory.clone(),
                        "sqpack".to_string(),
                        repository.name.clone(),
                    ]
                    .iter()
                    .collect();

                    fs::remove_dir_all(&repo_path)
                        .ok()
                        .ok_or(RepairError::FailedRepair(repository))?;

                    fs::create_dir_all(&repo_path)
                        .ok()
                        .ok_or(RepairError::FailedRepair(repository))?;

                    "2012.01.01.0000.0000".to_string() // TODO: is this correct for expansions?
                }
                RepairAction::VersionFileCanRestore => {
                    let ver_bak_path: PathBuf = [
                        self.game_directory.clone(),
                        "sqpack".to_string(),
                        repository.name.clone(),
                        format!("{}.bck", repository.name),
                    ]
                    .iter()
                    .collect();

                    read_version(&ver_bak_path).ok_or(RepairError::FailedRepair(repository))?
                }
            };

            fs::write(ver_path, new_version)
                .ok()
                .ok_or(RepairError::FailedRepair(repository))?;
        }

        Ok(())
    }

    fn cache_index_file(&mut self, filename: &str)  {
        if !self.index_files.contains_key(filename) {
            self.index_files.insert(filename.to_string(), IndexFile::from_existing(filename).unwrap());
        }
    }

    fn get_index_file(&self, filename: &str) -> Option<&IndexFile> {
        self.index_files.get(filename)
    }
}

#[cfg(test)]
mod tests {
    use crate::repository::Category::EXD;

    use super::*;

    fn common_setup_data() -> GameData {
        let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("resources/tests");
        d.push("valid_sqpack");
        d.push("game");

        GameData::from_existing(d.to_str().unwrap()).unwrap()
    }

    #[test]
    fn repository_ordering() {
        let mut data = common_setup_data();
        data.reload_repositories();

        assert_eq!(data.repositories[0].name, "ffxiv");
        assert_eq!(data.repositories[1].name, "ex1");
        assert_eq!(data.repositories[2].name, "ex2");
    }

    #[test]
    fn repository_and_category_parsing() {
        let mut data = common_setup_data();
        data.reload_repositories();

        assert_eq!(
            data.parse_repository_category("exd/root.exl").unwrap(),
            (&data.repositories[0], EXD)
        );
        assert!(data
            .parse_repository_category("what/some_font.dat")
            .is_none());
    }
}
