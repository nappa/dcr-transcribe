pub trait Sink: Send + 'static {
    fn start(&mut self) {}
    fn write(&mut self, chunk: &Chunk);
    fn stop(&mut self) {}
}
