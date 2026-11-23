use super::*;
use std::os::unix::net::UnixStream;

fn transfer_pair() -> (File, File) {
    let (read, write) = UnixStream::pair().unwrap();
    (File::from(OwnedFd::from(read)), File::from(OwnedFd::from(write)))
}

#[test]
fn reads_up_to_the_cap_and_refuses_more() {
    for length in [100, 101] {
        let (read, mut write) = transfer_pair();
        write.write_all(&vec![7; length]).unwrap();
        drop(write);
        let result = read_with_timeout(read, 100);
        if length == 100 {
            assert_eq!(result.unwrap(), vec![7; 100]);
        } else {
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
        }
    }
}

#[test]
fn a_slow_trickle_still_ends_at_the_deadline() {
    let (read, mut write) = transfer_pair();
    let writer = std::thread::spawn(move || {
        for _ in 0..40 {
            if write.write_all(b"x").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    let error = read_within(read, 1 << 20, Duration::from_millis(300)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    writer.join().unwrap();
}

#[test]
fn an_expired_deadline_takes_precedence_over_ready_data() {
    let (read, mut write) = transfer_pair();
    write.write_all(b"ready").unwrap();
    assert_eq!(read_within(read, 100, Duration::ZERO).unwrap_err().kind(), ErrorKind::TimedOut);
}

#[test]
fn a_peer_that_keeps_the_pipe_open_without_reading_cannot_block_a_send() {
    let (_read, write) = transfer_pair();
    let bytes = vec![7; 2 << 20];
    let error = write_with_idle_timeout(write, &bytes, Duration::from_millis(50)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
}

#[test]
fn a_slow_reader_receives_the_whole_selection_while_the_send_keeps_moving() {
    let (mut read, write) = transfer_pair();
    let text = vec![b'x'; 4 << 20];
    let reader = std::thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut received = Vec::new();
        let mut chunk = [0; 64 << 10];
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let count = read.read(&mut chunk)?;
            if count == 0 {
                return Ok(received);
            }
            received.extend_from_slice(&chunk[..count]);
        }
    });
    let idle_limit = Duration::from_millis(500);
    let start = Instant::now();
    let result = write_with_idle_timeout(write, &text, idle_limit);
    let received = reader.join().unwrap().unwrap();

    assert!(start.elapsed() > idle_limit);
    result.unwrap();
    assert_eq!(received, text);
}
