# SWQOS Router - Solana Transaction Proxy with Jito Integration

A high-performance Rust-based proxy server for Solana transactions that intelligently routes transactions to multiple RPC endpoints and integrates with Jito Block Engine for high-tip transactions.

## Features

### Core Functionality
- **Transaction Filtering**: Monitors incoming `sendTransaction` calls for specific target transfers
- **Multi-RPC Forwarding**: Distributes transactions across multiple Solana RPC endpoints
- **Tip-Based Routing**: Intelligently routes high-value transactions to specialized infrastructure

### Jito Integration
- **Automatic Bundle Creation**: High-tip transactions (≥1M lamports) are automatically bundled for Jito Block Engine
- **Tip Optimization**: Automatically reduces bundle tips by 100K lamports from original transaction tips
- **MEV Protection**: Ensures atomic execution of transaction bundles through Jito's infrastructure

### Advanced Features
- **Real-time Transaction Analysis**: Parses Solana transactions to extract transfer instructions
- **Configurable Thresholds**: Customizable tip thresholds and target parameters
- **Comprehensive Logging**: Detailed tracing for debugging and monitoring
- **Error Handling**: Robust error handling with detailed error responses

## Configuration

### Command Line Arguments

```bash
swqos-router [OPTIONS]

Options:
  -a, --addresses <ADDRESSES>           Comma-separated list of RPC endpoints to forward to
  -p, --port <PORT>                    Port to listen on [default: 8080]
      --jito-server <JITO_SERVER>      Jito block engine server address 
                                       [default: https://mainnet.block-engine.jito.wtf]
      --jito-signer-key <JITO_SIGNER_KEY>  Path to Jito signer private key file (base58 encoded)
      --enable-jito                    Enable Jito forwarding for high-tip transactions
  -h, --help                           Print help
  -V, --version                        Print version
```

### Environment Setup

1. **Install Rust**: Ensure you have Rust installed (1.70.0 or later recommended)
2. **Clone Repository**: 
   ```bash
   git clone <repository-url>
   cd swqos-router
   ```
3. **Build Project**: 
   ```bash
   cargo build --release
   ```

### Jito Configuration

To enable Jito integration:

1. **Generate Keypair**: Create a Solana keypair for Jito signing:
   ```bash
   solana-keygen new --outfile jito-signer.key --no-bip39-passphrase
   ```

2. **Fund the Account**: Ensure the Jito signer account has sufficient SOL for tip transactions

3. **Configure Key File**: The key file should contain the base58-encoded private key

## Usage Examples

### Basic Transaction Forwarding
```bash
# Forward transactions to multiple RPC endpoints
./swqos-router -a "https://rpc1.solana.com,https://rpc2.solana.com" -p 8080
```

### With Jito Integration
```bash
# Enable Jito forwarding for high-tip transactions
./swqos-router \
  -a "https://rpc1.solana.com,https://rpc2.solana.com" \
  -p 8080 \
  --enable-jito \
  --jito-signer-key ./jito-signer.key \
  --jito-server https://mainnet.block-engine.jito.wtf
```

### Docker Deployment
```dockerfile
FROM rust:1.70 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=builder /app/target/release/swqos-router .
COPY jito-signer.key .
CMD ["./swqos-router", "-a", "https://rpc.solana.com", "--enable-jito", "--jito-signer-key", "./jito-signer.key"]
```

## Transaction Flow

### Standard Flow (Tip < 1M lamports)
1. Client sends `sendTransaction` request
2. Router analyzes transaction for target transfers
3. If target transfer found → Forward to all configured RPC endpoints
4. Return transaction signature to client

### Jito-Enhanced Flow (Tip ≥ 1M lamports)
1. Client sends `sendTransaction` request with high tip
2. Router analyzes transaction and detects high-value tip
3. **Immediate Forwarding**: Transaction sent to regular RPC endpoints
4. **Jito Bundle Creation**:
   - Creates new tip transaction with reduced tip (original - 100K lamports)
   - Bundles original transaction + tip transaction
   - Submits bundle to Jito Block Engine
5. Return transaction signature to client

## API Specification

### Supported RPC Methods

#### sendTransaction
```json
{
  "jsonrpc": "2.0",
  "method": "sendTransaction",
  "params": ["<base64-encoded-transaction>", {...options}],
  "id": 1
}
```

**Response (Success)**:
```json
{
  "jsonrpc": "2.0",
  "result": "<transaction-signature>",
  "id": 1
}
```

**Response (No Target Transfer)**:
```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32603,
    "message": "Transaction does not contain target transfer",
    "data": {
      "err": "Missing Transfer"
    }
  },
  "id": 1
}
```

## Configuration Constants

| Constant | Default Value | Description |
|----------|---------------|-------------|
| `TARGET_PUBKEY` | `GxkB4oYYLsoeAoxAdXjDEBSrP7JGCy3re7mqozFYyiYW` | Target address for transfer detection |
| `TARGET_LAMPORTS` | `100,000` | Minimum lamports required in target transfer |
| `JITO_TIP_THRESHOLD` | `1,000,000` | Minimum tip for Jito bundle creation |
| `JITO_TIP_REDUCTION` | `100,000` | Amount to reduce from original tip for Jito bundle |

## Monitoring and Logging

### Log Levels
- **INFO**: General operation information, successful transactions
- **WARN**: Non-critical issues, failed forwarding attempts
- **ERROR**: Critical errors, transaction processing failures
- **DEBUG**: Detailed transaction analysis, parsing information

### Key Metrics to Monitor
- Transaction throughput
- Forward success rates
- Jito bundle success rates
- Response latencies
- Error rates by type

## Security Considerations

### Private Key Management
- Store Jito signer keys securely
- Use proper file permissions (600) for key files
- Consider using hardware security modules for production
- Rotate keys regularly

### Network Security
- Use HTTPS for all RPC endpoints
- Implement rate limiting if needed
- Monitor for unusual transaction patterns
- Validate all incoming transactions

### Operational Security
- Run with minimal required permissions
- Use dedicated accounts for Jito signing
- Monitor account balances
- Implement proper logging and alerting

## Troubleshooting

### Common Issues

#### "Jito enabled but no signer key provided"
**Solution**: Ensure you provide the `--jito-signer-key` parameter when using `--enable-jito`

#### "Failed to decode base58 keypair"
**Solution**: Verify the keypair file contains a valid base58-encoded private key

#### "No tip accounts available"
**Solution**: Check Jito Block Engine connectivity and ensure the service is operational

#### "Failed to send bundle to Jito"
**Solution**: 
- Verify Jito signer account has sufficient SOL balance
- Check network connectivity to Jito Block Engine
- Ensure tip amount meets minimum requirements

### Debug Mode
Run with debug logging:
```bash
RUST_LOG=debug ./swqos-router [OPTIONS]
```

## Performance Tuning

### Recommended Settings
- **Production**: 4+ CPU cores, 8GB+ RAM
- **Development**: 2+ CPU cores, 4GB+ RAM
- **Network**: Low latency connection to RPC endpoints

### Optimization Tips
- Use multiple RPC endpoints for redundancy
- Monitor RPC endpoint performance
- Adjust tip thresholds based on network conditions
- Use SSD storage for key files and logs

## Contributing

### Development Setup
```bash
# Clone repository
git clone <repository-url>
cd swqos-router

# Install dependencies
cargo build

# Run tests
cargo test

# Run with development logging
RUST_LOG=debug cargo run -- [OPTIONS]
```

### Code Standards
- Follow Rust naming conventions
- Include comprehensive error handling
- Add tests for new functionality
- Update documentation for API changes

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Support

For technical support and questions:
- Create an issue in the repository
- Check existing documentation
- Review logs for error details

## Changelog

### v0.1.0
- Initial release with basic transaction forwarding
- Jito Block Engine integration
- Multi-RPC endpoint support
- Configurable tip thresholds
- Comprehensive logging and error handling