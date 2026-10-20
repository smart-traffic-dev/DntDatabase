use dnt_testkit::{require_db, Target};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

fn reachable(port: u16) -> bool {
    let addr = ("127.0.0.1", port)
        .to_socket_addrs()
        .unwrap()
        .next()
        .unwrap();
    TcpStream::connect_timeout(&addr, Duration::from_secs(5)).is_ok()
}

#[test]
fn ports_reachable() {
    require_db!();
    for t in Target::all() {
        let port = t.resolved(None).port;
        assert!(reachable(port), "{t:?} not reachable on port {port}");
    }
    assert!(reachable(2222), "ssh not reachable on port 2222");
}

#[test]
fn target_matrix_is_consistent() {
    assert_eq!(Target::all().len(), 6);
    assert_eq!(Target::mysql_family().len() + Target::pg().len(), 6);
    assert_eq!(Target::MySql8.resolved(Some("dnt_fixture")).port, 33080);
    assert_eq!(Target::Pg17.ssh_target_host(), ("pg17", 5432));
    assert!(dnt_testkit::testdata("sql/mysql_common.sql").exists());
}
