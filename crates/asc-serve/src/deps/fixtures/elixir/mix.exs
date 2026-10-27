defmodule App.MixProject do
  use Mix.Project

  def project do
    [app: :app, version: "0.1.0", deps: deps()]
  end

  defp deps do
    [
      {:plug, "~> 1.14"},
      {:cowboy, "~> 2.9", only: :prod},
      {:my_fork, git: "https://github.com/example/my_fork.git"}
    ]
  end
end
