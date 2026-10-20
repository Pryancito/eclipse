use super::*;
use alloc::string::String;
use alloc::sync::Arc;

#[test]
fn o_direct_on_a_pipe_is_einval() {
    let (r, _w) = Pipe::create_pair();
    let like: Arc<dyn FileLike> =
        File::new(Arc::new(r), OpenFlags::RDONLY, String::from("pipe_r:[]"));
    assert_eq!(
        setfl_pipe_direct(&like, OpenFlags::RDONLY | OpenFlags::DIRECT),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        setfl_pipe_direct(&like, OpenFlags::RDONLY | OpenFlags::NON_BLOCK),
        Ok(())
    );
}
