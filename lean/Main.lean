import PetriModel.Wire

/-- Answer one JSON query per line on stdin until it closes. -/
def main : IO Unit := do
  let stdin ← IO.getStdin
  let stdout ← IO.getStdout
  repeat
    let line ← stdin.getLine
    if line.isEmpty then break
    stdout.putStrLn (PetriModel.Wire.respond line.trimAscii.toString)
    stdout.flush
