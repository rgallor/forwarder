# forwarder
Simple application allowing port forwarding on a WebSocket communication between a host (bridge) and a device

## Run the application

To run the application is necessary to:
* have a Virtual Machine to run either the Bridge or the Device
* install [TTYD](https://github.com/tsl0922/ttyd) command-line tool to share a terminal over the web on the machine where you want to run the Device

Respectively use the scripts `start_bridge.sh` and `start_device.sh` to run either the Bridge or the Device. The former requires to pass the address (IP and port) the Bridge will use to listen for WebSocket connections and another address the Bridge will listen for browser connection. The last requires the url (scheme, IP, and port) the Device will use to opena a WebSocket connection with the Bridge.

