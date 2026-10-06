import yaml

with open('../docker-compose.yml', 'r') as f:
    compose = yaml.safe_load(f)

# Update hmm_bot volumes to map correctly for the Rust bot
volumes = compose['services']['hmm_bot']['volumes']
new_volumes = []
for vol in volumes:
    if 'binance_hmm' in vol:
        # The rust bot writes to ../ which resolves to / inside the container when WORKDIR is /app
        host_path = vol.split(':')[0]
        file_name = host_path.split('/')[-1]
        new_volumes.append(f"{host_path}:/{file_name}")
    else:
        new_volumes.append(vol)

# Also persist hmm weights
new_volumes.append("./rust-bot/hmm_weights.json:/app/hmm_weights.json")

compose['services']['hmm_bot']['volumes'] = new_volumes

# Remove the --run-now arg since we don't need it
compose['services']['hmm_bot']['command'] = ["trading-bot"]

with open('../docker-compose.yml', 'w') as f:
    yaml.dump(compose, f, sort_keys=False)

