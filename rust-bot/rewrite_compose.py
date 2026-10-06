import yaml

with open('../docker-compose.yml', 'r') as f:
    compose = yaml.safe_load(f)

# Keep only dashboard if possible, remove old bot and hmm_bot
new_services = {}

# Dashboard can stay, but we will remove old state mounts
dashboard = compose['services'].get('dashboard')
if dashboard:
    # Just remove volumes entirely so it doesn't crash due to missing files
    dashboard['volumes'] = []
    new_services['dashboard'] = dashboard

# Create BTC bot
new_services['hmm_bot_btc'] = {
    'build': './rust-bot',
    'container_name': 'hmm_bot_btc',
    'restart': 'unless-stopped',
    'command': ['trading-bot'],
    'env_file': ['.env'],
    'environment': ['TRADING_SYMBOL=BTCUSDC'],
    'volumes': ['./rust-bot_data:/app']
}

# Create ETH bot
new_services['hmm_bot_eth'] = {
    'build': './rust-bot',
    'container_name': 'hmm_bot_eth',
    'restart': 'unless-stopped',
    'command': ['trading-bot'],
    'env_file': ['.env'],
    'environment': ['TRADING_SYMBOL=ETHUSDC'],
    'volumes': ['./rust-bot_data:/app']
}

# Create SOL bot
new_services['hmm_bot_sol'] = {
    'build': './rust-bot',
    'container_name': 'hmm_bot_sol',
    'restart': 'unless-stopped',
    'command': ['trading-bot'],
    'env_file': ['.env'],
    'environment': ['TRADING_SYMBOL=SOLUSDC'],
    'volumes': ['./rust-bot_data:/app']
}

compose['services'] = new_services
# Remove the old obsolete version string
if 'version' in compose:
    del compose['version']

with open('../docker-compose.yml', 'w') as f:
    yaml.dump(compose, f, sort_keys=False)

