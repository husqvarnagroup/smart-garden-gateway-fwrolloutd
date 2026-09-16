// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use async_compression::tokio::write::GzipEncoder;
use async_tar::{Builder, Header};
use fwrolloutd::IpsoRegistry;
use lazy_static::lazy_static;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use tempdir::TempDir;
use tokio::io::AsyncWriteExt;

use url::Url;

use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

lazy_static! {
        /* IPSO definitions on GW image */
        static ref IPSO_ARCHIVE_BASE: HashMap<&'static str, &'static str> = {
        let mut m = HashMap::new();
        m.insert("ipso1.xml", "ipso1");
        m.insert("ipso2.xml", "ipso2");
        m.insert("ipso3.xml", "ipso3");
        m
    };

    /* IPSO Image downloaded: version A */
    static ref IPSO_ARCHIVE_A: HashMap<&'static str, &'static str> = {
        let mut m = HashMap::new();
        m.insert("ipso1.xml", "ipso1");
        m.insert("ipso3.xml", "ipso3\nnew additional data");
        m.insert("ipso4.xml", "ipso4");
        m
    };

    /* IPSO Image downloaded: version B */
    static ref IPSO_ARCHIVE_B: HashMap<&'static str, &'static str> = {
        let mut m = HashMap::new();
        m.insert("ipso1.xml", "ipso1");
        m.insert("ipso3.xml", "ipso3\nnew additional data\nmore new data");
        m.insert("ipso5.xml", "ipso5");
        m
    };
}

const IPSO_ARCHIVE_TEST_NAME_0: &str =
    "ipso_definitions_9630f6ea6a0608b03bfa279b340957fe68afcf4c.tar.gz";
const IPSO_ARCHIVE_TEST_NAME_1: &str =
    "ipso_definitions_ff01234567890abcdef0123456789abcdeffffff.tar.gz";

#[tokio::test]
async fn test_get_ipso_archive_with_download_ok_overwrite() {
    let uri = setup_mock_server_with_reply_ok(IPSO_ARCHIVE_TEST_NAME_0, &IPSO_ARCHIVE_A).await;

    let tmp_dir = create_temp_registry_directory();
    create_temp_registry_hash_file(&tmp_dir, IPSO_ARCHIVE_TEST_NAME_1);

    let ipso_registry = IpsoRegistry::new(tmp_dir.path());
    ipso_registry.update_if_needed(&uri).await.unwrap();

    let expected_files = HashMap::from([
        ("archive_name", IPSO_ARCHIVE_TEST_NAME_0),
        ("ipso3.xml", "ipso3\nnew additional data"),
        ("ipso4.xml", "ipso4"),
    ]);
    assert_fwrollout_directory_content(&tmp_dir, &expected_files);
    assert_base_directory(&tmp_dir);
}

#[tokio::test]
async fn test_get_ipso_archive_with_reply_ok_no_previous_registry() {
    let uri = setup_mock_server_with_reply_ok(IPSO_ARCHIVE_TEST_NAME_0, &IPSO_ARCHIVE_A).await;

    let tmp_dir = create_temp_registry_directory();

    let ipso_registry = IpsoRegistry::new(tmp_dir.path());
    ipso_registry.update_if_needed(&uri).await.unwrap();

    let expected_files = HashMap::from([
        ("archive_name", IPSO_ARCHIVE_TEST_NAME_0),
        ("ipso3.xml", "ipso3\nnew additional data"),
        ("ipso4.xml", "ipso4"),
    ]);
    assert_fwrollout_directory_content(&tmp_dir, &expected_files);
    assert_base_directory(&tmp_dir);
}

#[tokio::test]
async fn test_get_ipso_archive_with_already_up_to_date() {
    let uri = setup_mock_server_assure_not_called().await;

    let tmp_dir = create_temp_registry_directory();
    create_temp_registry_hash_file(&tmp_dir, IPSO_ARCHIVE_TEST_NAME_1);

    let ipso_registry = IpsoRegistry::new(tmp_dir.path());
    ipso_registry.update_if_needed(&uri).await.unwrap();

    let expected_files = HashMap::from([("archive_name", IPSO_ARCHIVE_TEST_NAME_1)]);
    assert_fwrollout_directory_content(&tmp_dir, &expected_files);
    assert_base_directory(&tmp_dir);
}

#[tokio::test]
async fn test_get_ipso_archive_with_reply_ok_then_again() {
    let tmp_dir = create_temp_registry_directory();

    /* get first archive */
    let uri = setup_mock_server_with_reply_ok(IPSO_ARCHIVE_TEST_NAME_0, &IPSO_ARCHIVE_A).await;

    let ipso_registry = IpsoRegistry::new(tmp_dir.path());
    ipso_registry.update_if_needed(&uri).await.unwrap();

    let expected_files = HashMap::from([
        ("archive_name", IPSO_ARCHIVE_TEST_NAME_0),
        ("ipso3.xml", "ipso3\nnew additional data"),
        ("ipso4.xml", "ipso4"),
    ]);
    assert_fwrollout_directory_content(&tmp_dir, &expected_files);
    assert_base_directory(&tmp_dir);

    /* get second archive */
    let uri = setup_mock_server_with_reply_ok(IPSO_ARCHIVE_TEST_NAME_1, &IPSO_ARCHIVE_B).await;

    let ipso_registry = IpsoRegistry::new(tmp_dir.path());

    ipso_registry.update_if_needed(&uri).await.unwrap();

    let expected_files = HashMap::from([
        ("archive_name", IPSO_ARCHIVE_TEST_NAME_1),
        ("ipso3.xml", "ipso3\nnew additional data\nmore new data"),
        ("ipso5.xml", "ipso5"),
    ]);
    assert_fwrollout_directory_content(&tmp_dir, &expected_files);
    assert_base_directory(&tmp_dir);
}

fn create_temp_registry_directory() -> TempDir {
    let tmp_dir = TempDir::new("lwm2m_registry").expect("Can't create temp dir for testing");
    let tmp_dir_path = tmp_dir.path().to_path_buf();
    let tmp_dir_base = tmp_dir_path.join("base");
    fs::create_dir(&tmp_dir_base).expect("Can't create temp base dir");

    /* populate base dir */
    for (file_name, file_content) in IPSO_ARCHIVE_BASE.iter() {
        let filename = tmp_dir_base.join(file_name);
        fs::write(filename, file_content).expect("Can't write IPSO for tests");
    }
    assert_base_directory(&tmp_dir);
    // need to keep a reference to `tmp_dir` for RAII
    tmp_dir
}

fn create_temp_registry_hash_file(tmp_dir: &TempDir, ipso_archive_name: &str) {
    let registry_path = &tmp_dir.path().to_path_buf();
    fs::create_dir_all(registry_path.join("fwrolloutd"))
        .expect("Could not create IPSO directory folder");
    File::create(registry_path.join("fwrolloutd").join("archive_name"))
        .expect("Can't create archive_name file for testing")
        .write_all(ipso_archive_name.as_bytes())
        .expect("Can't write archive_name data to file for testing");
}

async fn setup_mock_server_with_reply_ok(
    archive_name: &str,
    archive_to_send: &HashMap<&str, &str>,
) -> Url {
    let mock_server = MockServer::start().await;

    let buf = create_test_ipso_archive(archive_to_send).await;

    Mock::given(method("GET"))
        .and(path(archive_name))
        .respond_with(ResponseTemplate::new(200).set_body_raw(buf, "application/tar+gzip"))
        .mount(&mock_server)
        .await;

    create_fake_archive_url(mock_server, archive_name)
}

async fn setup_mock_server_assure_not_called() -> Url {
    let mock_server = MockServer::start().await;

    Mock::given(any())
        .respond_with(ResponseTemplate::new(501))
        .expect(0)
        .mount(&mock_server)
        .await;

    create_fake_archive_url(mock_server, IPSO_ARCHIVE_TEST_NAME_1)
}

async fn create_test_ipso_archive(archive_content: &HashMap<&str, &str>) -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());

    for (file_name, file_content) in archive_content.iter() {
        let mut header = Header::new_gnu();
        header.set_path(file_name).unwrap();
        header.set_size(file_content.len() as u64);
        header.set_cksum();
        builder
            .append(&header, file_content.as_bytes())
            .await
            .expect("Can't build tar");
    }
    builder.finish().await.expect("Can't finish tar");

    let mut gz = GzipEncoder::new(Vec::new());
    gz.write_all(&builder.into_inner().await.expect("Can't get tar for gz"))
        .await
        .expect("Can't gz");
    gz.shutdown().await.expect("Can't compress tar file");

    gz.into_inner()
}

fn create_fake_archive_url(mock_server: MockServer, file_name: &str) -> Url {
    Url::parse(mock_server.uri().as_str())
        .expect("Can't parse mock server uri")
        .join(file_name)
        .expect("Can't join URI for IPSO archive")
}

fn assert_base_directory(root_dir: &TempDir) {
    let root_dir = root_dir.path().to_path_buf();
    let base_dir = Path::new(&root_dir).join("base");
    assert_directory_content(&base_dir, &IPSO_ARCHIVE_BASE);
}

fn assert_fwrollout_directory_content(root_dir: &TempDir, expected_content: &HashMap<&str, &str>) {
    let root_dir = root_dir.path().to_path_buf();
    let base_dir = Path::new(&root_dir).join("fwrolloutd");
    assert_directory_content(&base_dir, expected_content);
}

fn assert_directory_content(base_dir: &Path, expected_content: &HashMap<&str, &str>) {
    let all_entries = fs::read_dir(base_dir)
        .expect("Can't read base directory")
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect::<HashSet<_>>();
    let expected_entries: HashSet<_> = expected_content.keys().map(|e| e.to_string()).collect();
    assert_eq!(
        all_entries, expected_entries,
        "Actual {:?} not equal to expected {:?}",
        all_entries, expected_entries
    );
    for (path, expected_content) in expected_content.iter() {
        let file_content =
            fs::read_to_string(base_dir.join(path)).expect("Can't read file contents");
        assert_eq!(
            file_content,
            expected_content.to_string(),
            "File content of {} is not as expected",
            path
        );
    }
}
