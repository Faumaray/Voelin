fn main() {
    let pool = schema_api::reflection::descriptor_pool();
    for service in pool.services() {
        println!("{}", service.full_name());
        for method in service.methods() {
            println!(
                "  {}({}{}) -> {}{}",
                method.name(),
                if method.is_client_streaming() {
                    "stream "
                } else {
                    ""
                },
                method.input().full_name(),
                if method.is_server_streaming() {
                    "stream "
                } else {
                    ""
                },
                method.output().full_name(),
            );
        }
    }
}
