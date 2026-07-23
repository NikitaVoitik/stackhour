package main

import (
	"embed"
	"log"

	"github.com/wailsapp/wails/v2"
	"github.com/wailsapp/wails/v2/pkg/options"
	"github.com/wailsapp/wails/v2/pkg/options/assetserver"
)

//go:embed all:dist
var assets embed.FS

func main() {
	app := newApp()
	err := wails.Run(&options.App{
		Title:            "Stackhour UI Benchmark",
		Width:            1280,
		Height:           800,
		MinWidth:         1280,
		MinHeight:        800,
		MaxWidth:         1280,
		MaxHeight:        800,
		DisableResize:    true,
		Frameless:        true,
		BackgroundColour: &options.RGBA{R: 0x17, G: 0x19, B: 0x1f, A: 0xff},
		AssetServer:      &assetserver.Options{Assets: assets},
		OnStartup:        app.startup,
		OnShutdown:       app.shutdown,
		Bind:             []interface{}{app},
	})
	if err != nil {
		log.Fatal(err)
	}
}
