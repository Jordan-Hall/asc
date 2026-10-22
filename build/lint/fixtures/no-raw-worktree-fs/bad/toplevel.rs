fn read(ws: &WsCtx) {
    let _ = std::fs::read(ws.projection.toplevel().join(".gitattributes"));
}
