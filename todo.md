# TODO

## Design
This will be a rust (tui) version of Pi (https://github.com/earendil-works/pi)
We only need to support local models using the openai-completions api type. See ~/.pi/agent/models.json for a model you can test with. You are running on the same API, so you can't run concurrent requests.

Use the same system prompt and tools. We don't need an extension system. Config should all be in the same .yaml file (~/.config/rho/config.yaml).

## Addidional features
- Discord remote control
- /loop that goes 
    - forever
    - a number of times
    - duration
    - checks a condition with a prompt

## Process
You will create the design and store it in DESIGN.md. Include in the design the steps you will need to do to complete the project. Then implement the design following the steps you created. Committing each along the way.
