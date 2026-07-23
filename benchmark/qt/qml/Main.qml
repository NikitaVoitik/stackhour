import QtQuick

Window {
    id: root
    width: 1280
    height: 800
    minimumWidth: 1280
    maximumWidth: 1280
    minimumHeight: 800
    maximumHeight: 800
    visible: true
    flags: Qt.Window | Qt.FramelessWindowHint
    color: "#17191f"
    title: "Stackhour UI Benchmark"

    readonly property string uiFont: "Noto Sans"
    readonly property string monoFont: "Noto Sans Mono"

    Rectangle { x: 0; y: 0; width: 48; height: 776; color: "#1d2027" }
    Rectangle { x: 48; y: 0; width: 260; height: 776; color: "#1c1f26" }
    Rectangle { x: 307; y: 0; width: 1; height: 776; color: "#30343e" }
    Rectangle { x: 47; y: 0; width: 1; height: 776; color: "#30343e" }
    Rectangle { x: 308; y: 0; width: 972; height: 36; color: "#1b1e24" }
    Rectangle { x: 308; y: 35; width: 972; height: 1; color: "#30343e" }
    Rectangle { x: 1060; y: 36; width: 220; height: 740; color: "#1b1e24" }
    Rectangle { x: 1059; y: 36; width: 1; height: 740; color: "#30343e" }
    Rectangle { x: 308; y: 636; width: 752; height: 140; color: "#181a20" }
    Rectangle { x: 308; y: 635; width: 752; height: 1; color: "#30343e" }
    Rectangle { x: 0; y: 776; width: 1280; height: 24; color: "#255a91" }

    Repeater {
        model: ["⌘", "⌕", "⑂", "△"]
        delegate: Rectangle {
            required property string modelData
            required property int index
            x: 8
            y: 10 + index * 40
            width: 32
            height: 32
            color: index === 0 ? "#292d36" : "transparent"
            Text {
                anchors.centerIn: parent
                text: parent.modelData
                color: index === 0 ? "#f3f5f8" : "#8c93a3"
                font.family: root.uiFont
                font.pixelSize: 17
            }
        }
    }

    Text {
        x: 60; y: 0; width: 248; height: 36
        verticalAlignment: Text.AlignVCenter
        text: "EXPLORER · CONTROLLED FIXTURE"
        color: "#aab0bd"; font.family: root.uiFont; font.pixelSize: 11
    }

    ListView {
        id: tree
        x: 48; y: 36; width: 260; height: 720
        clip: true
        interactive: false
        reuseItems: true
        cacheBuffer: 72
        model: backend.files
        contentY: backend.treeTop * 18
        delegate: Rectangle {
            required property string modelData
            required property int index
            width: tree.width; height: 18
            color: modelData === backend.selected ? "#2c3240" : "transparent"
            Text {
                x: 12; width: parent.width - 12; height: 18
                verticalAlignment: Text.AlignVCenter
                text: "<span style='color:#e2bd75'>◇</span> " + parent.modelData.split("/").pop()
                textFormat: Text.RichText
                color: "#b7bdc9"; font.family: root.monoFont; font.pixelSize: 13
            }
        }
    }

    Rectangle {
        x: 308; y: 0; width: 190; height: 36; color: "#17191f"
        Text {
            x: 14; height: parent.height; verticalAlignment: Text.AlignVCenter
            text: "<span style='color:#4aa5f0;font-weight:600'>TS</span>   "
                  + backend.selected.split("/").pop() + "                    ×"
            textFormat: Text.RichText
            color: "#f3f5f8"; font.family: root.uiFont; font.pixelSize: 13
        }
    }
    Text {
        x: 512; y: 0; width: 190; height: 36; verticalAlignment: Text.AlignVCenter
        text: "<span style='color:#4aa5f0;font-weight:600'>TS</span>   alternate.ts"
        textFormat: Text.RichText
        color: "#aeb4bf"; font.family: root.uiFont; font.pixelSize: 13
    }

    ListView {
        id: editor
        x: 308; y: 36; width: 752; height: 600
        clip: true
        interactive: false
        reuseItems: true
        cacheBuffer: 80
        model: backend.lines
        contentY: backend.editorTop * 20
        delegate: Item {
            required property string modelData
            required property int index
            width: editor.width; height: 20
            Text {
                x: 0; width: 44; height: 20
                horizontalAlignment: Text.AlignRight; verticalAlignment: Text.AlignVCenter
                text: index + 1
                color: "#565d6c"; font.family: root.monoFont; font.pixelSize: 13
            }
            Text {
                x: 58; width: parent.width - 58; height: 20
                verticalAlignment: Text.AlignVCenter
                text: backend.highlight(parent.modelData)
                textFormat: Text.RichText
                color: "#c7cbd4"; font.family: root.monoFont; font.pixelSize: 13
            }
        }
    }

    Text {
        x: 1072; y: 36; width: 208; height: 36; verticalAlignment: Text.AlignVCenter
        text: "OUTLINE"; color: "#aab0bd"; font.family: root.uiFont; font.pixelSize: 11
    }
    ListView {
        x: 1068; y: 72; width: 204; height: 528
        clip: true; interactive: false
        model: backend.symbols.length ? backend.symbols.slice(0, 24) : [{name: "Initializing TypeScript…"}]
        delegate: Text {
            required property var modelData
            width: 204; height: 22; verticalAlignment: Text.AlignVCenter
            text: (backend.symbols.length ? "<span style='color:#c792ea'>◇</span> " : "") + modelData.name
            textFormat: Text.RichText
            color: "#aeb4bf"; font.family: root.monoFont; font.pixelSize: 12
        }
    }

    Rectangle { x: 308; y: 667; width: 752; height: 1; color: "#282c35" }
    Text {
        x: 320; y: 636; width: 400; height: 31; verticalAlignment: Text.AlignVCenter
        text: "OUTPUT        PROBLEMS        TERMINAL"
        color: "#d6d9df"; font.family: root.uiFont; font.pixelSize: 11
    }
    Text {
        x: 320; y: 675; width: 720; height: 70
        text: "[benchmark] deterministic project fixture\n[scanner] " + backend.scannerText
              + "\n[typescript] " + backend.languageText
        color: "#8f97a6"; font.family: root.monoFont; font.pixelSize: 12; lineHeight: 1.66
    }
    Text {
        x: 10; y: 776; width: 500; height: 24; verticalAlignment: Text.AlignVCenter
        text: "⑂ benchmark/common    ✓ 0    ⚠ 0"
        color: "white"; font.family: root.uiFont; font.pixelSize: 12
    }
    Text {
        x: 760; y: 776; width: 510; height: 24
        horizontalAlignment: Text.AlignRight; verticalAlignment: Text.AlignVCenter
        text: "Ln 1, Col 1    Spaces: 2    UTF-8    TypeScript"
        color: "white"; font.family: root.uiFont; font.pixelSize: 12
    }
}
