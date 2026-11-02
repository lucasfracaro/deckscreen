// DeckScreen — widget do KDE Plasma 6 (bandeja do sistema).
// Fala só com o serviço local em 127.0.0.1:47557 (rotas /ui/*).
import QtQuick
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.components as PlasmaComponents
import org.kde.kirigami as Kirigami

PlasmoidItem {
    id: root

    property var st: null
    readonly property bool online: st !== null
    readonly property bool active: online && st.display !== undefined && st.display.active === true
    readonly property var pending: (online && st.pending) ? st.pending : []

    Plasmoid.icon: "video-display"
    Plasmoid.status: (active || pending.length > 0) ? PlasmaCore.Types.ActiveStatus : PlasmaCore.Types.PassiveStatus
    toolTipMainText: "DeckScreen"
    toolTipSubText: statusText()

    function statusText() {
        if (!online)
            return "O serviço deckscreen-host não está respondendo.";
        if (pending.length > 0)
            return pending[0].device_name + " quer se conectar (PIN " + pending[0].pin + ")";
        if (active && st.display.session) {
            var s = st.display.session;
            return "Steam Deck conectado · " + s.width + "×" + s.height + " @ " + s.fps + " · " + s.mode;
        }
        return "Aguardando o Steam Deck";
    }

    function request(method, path, body, done) {
        var x = new XMLHttpRequest();
        x.open(method, "http://127.0.0.1:47557" + path);
        x.setRequestHeader("X-DeckScreen-UI", "1");
        if (body)
            x.setRequestHeader("Content-Type", "application/json");
        x.onreadystatechange = function () {
            if (x.readyState === XMLHttpRequest.DONE && done)
                done(x.status, x.responseText);
        };
        x.send(body ? JSON.stringify(body) : null);
    }

    function refresh() {
        request("GET", "/ui/status", null, function (code, text) {
            if (code === 200) {
                try { root.st = JSON.parse(text); } catch (e) { root.st = null; }
            } else {
                root.st = null;
            }
        });
    }

    function decide(id, accept) {
        request("POST", "/ui/decide", { "id": id, "accept": accept }, function () { root.refresh(); });
    }

    Timer {
        interval: (root.active || root.expanded) ? 2000 : 5000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: root.refresh()
    }

    compactRepresentation: MouseArea {
        onClicked: root.expanded = !root.expanded
        Kirigami.Icon {
            anchors.fill: parent
            source: "video-display"
            opacity: root.active ? 1.0 : 0.55
        }
    }

    fullRepresentation: ColumnLayout {
        Layout.preferredWidth: Kirigami.Units.gridUnit * 18
        Layout.minimumHeight: Kirigami.Units.gridUnit * 6
        spacing: Kirigami.Units.smallSpacing

        Kirigami.Heading {
            level: 3
            text: "DeckScreen"
        }

        PlasmaComponents.Label {
            Layout.fillWidth: true
            wrapMode: Text.WordWrap
            text: root.statusText()
        }

        PlasmaComponents.Label {
            Layout.fillWidth: true
            wrapMode: Text.WordWrap
            visible: root.online && root.st.display && root.st.display.last_error
            color: Kirigami.Theme.negativeTextColor
            text: (root.online && root.st.display && root.st.display.last_error) ? root.st.display.last_error : ""
        }

        Repeater {
            model: root.pending
            delegate: RowLayout {
                required property var modelData
                PlasmaComponents.Label {
                    Layout.fillWidth: true
                    text: modelData.device_name + " · PIN " + modelData.pin
                }
                PlasmaComponents.Button {
                    text: "Aceitar"
                    icon.name: "dialog-ok-apply"
                    onClicked: root.decide(modelData.id, true)
                }
                PlasmaComponents.Button {
                    text: "Recusar"
                    icon.name: "dialog-cancel"
                    onClicked: root.decide(modelData.id, false)
                }
            }
        }

        PlasmaComponents.Button {
            Layout.fillWidth: true
            text: "Desconectar o Steam Deck"
            icon.name: "media-playback-stop"
            enabled: root.active
            onClicked: root.request("POST", "/ui/stop", {}, function () { root.refresh(); })
        }
    }
}
